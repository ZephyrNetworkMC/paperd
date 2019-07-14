// This file is part of paperd, the PaperMC server daemon
// Copyright (C) 2019 Kyle Wood (DemonWav)
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Lesser General Public License as published by
// the Free Software Foundation, version 3 only.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Lesser General Public License for more details.
//
// You should have received a copy of the GNU Lesser General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use nix::errno::Errno;
use nix::libc::{ftok, key_t};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::cmp::min;
use std::ffi::CString;
use std::mem::size_of;
use std::os::raw::{c_long, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process;
use std::ptr::null_mut;
use std::str::from_utf8;

mod libc {
    use nix::libc::{key_t, size_t, ssize_t};
    use std::os::raw::{c_int, c_long, c_void};

    pub const IPC_CREAT: c_int = 0o1000;
    pub const IPC_RMID: c_int = 0;

    #[cfg_attr(rustfmt, rustfmt_skip)]
    extern "C" {
        pub fn msgctl(msqid: c_int, cmd: c_int, buf: *mut c_void) -> c_int;
        pub fn msgget(key: key_t, msgflg: c_int) -> c_int;
        pub fn msgrcv(msqid: c_int, msgp: *mut c_void, msgsz: size_t, msgtyp: c_long, msgflg: c_int) -> ssize_t;
        pub fn msgsnd(msqid: c_int, msgp: *const c_void, msgsz: size_t, msgflg: c_int) -> c_int;
    }
}

pub fn open_message_channel<P: AsRef<Path>>(pid_file: P) -> Result<MessageChannel, i32> {
    let pid_file = pid_file.as_ref();
    let pid_file = match pid_file.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            eprintln!("Failed to canonicalize {}", pid_file.to_string_lossy());
            return Err(1);
        }
    };

    let file_name = match CString::new(pid_file.as_os_str().to_os_string().as_bytes()) {
        Ok(s) => s,
        Err(_) => {
            eprintln!(
                "Failed to initialize message channel for {}",
                pid_file.to_string_lossy()
            );
            return Err(1);
        }
    };

    let msq_id: i32 = unsafe {
        let msg_key = ftok(file_name.as_ptr(), 'P' as i32);
        libc::msgget(msg_key, 0o666 | libc::IPC_CREAT)
    };

    if msq_id == -1 {
        let msg = Errno::last().desc();
        eprintln!("Failed to open message channel: {}: {}", msq_id, msg);
        return Err(1);
    }

    return Ok(MessageChannel { msq_id });
}

const MESSAGE_TYPE: c_long = 0x7654;
const MESSAGE_LENGTH: usize = 100;

#[repr(C)]
struct Message {
    m_type: c_long,
    data: Data,
}

#[repr(C)]
struct Data {
    response_chan: i32,
    response_pid: u32,
    message_type: i16,
    message_length: u8,
    message: [u8; MESSAGE_LENGTH],
}

pub trait MessageHandler {
    fn type_id() -> i16;
    fn expect_response() -> bool;
}

pub struct MessageChannel {
    msq_id: i32,
}

#[derive(Deserialize)]
struct ServerErrorMessage {
    #[serde(rename = "error")]
    error: String,
}

impl MessageChannel {
    pub fn send_message<T>(&self, message: T) -> Result<Option<ResponseChannel>, i32>
    where
        T: MessageHandler + Serialize,
    {
        let exp_resp = T::expect_response();
        let receive_chan = if exp_resp {
            create_receive_channel()?
        } else {
            -1
        };

        let msg = match serde_json::to_string(&message) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to serialize JSON: {}", e);
                return Err(1);
            }
        };

        let mut data = msg.as_bytes();

        while data.len() > 0 {
            let size = min(data.len(), MESSAGE_LENGTH);
            let ret = self.send_paged_message(
                T::type_id(),
                receive_chan,
                &data[..size],
                size == data.len(),
            );

            if ret == -1 {
                let msg = Errno::last().desc();
                eprintln!(
                    "Failed to send message to channel: {}: {}",
                    self.msq_id, msg
                );
                return Err(1);
            }

            data = &data[size..];
        }

        if exp_resp {
            return Ok(Some(ResponseChannel {
                response_chan: receive_chan,
            }));
        }

        return Ok(None);
    }

    fn send_paged_message(&self, type_id: i16, receive_chan: i32, msg: &[u8], fin: bool) -> i32 {
        let mut message = Message {
            m_type: MESSAGE_TYPE,
            data: Data {
                response_chan: receive_chan,
                response_pid: process::id(),
                message_type: type_id,
                message_length: 0,
                message: [0; MESSAGE_LENGTH],
            },
        };

        let len = msg.len();
        {
            let message_slice = &mut message.data.message[..len];
            message_slice.copy_from_slice(msg);
        }

        let mut len = len as u8;
        if fin {
            // Set the far left bit to denote this is the end of a message
            len |= 0x80;
        }
        message.data.message_length = len;

        return unsafe {
            libc::msgsnd(
                self.msq_id,
                &mut message as *mut _ as *mut c_void,
                size_of::<Data>(),
                0,
            )
        };
    }
}

fn create_receive_channel() -> Result<i32, i32> {
    let pid = process::id();

    let msqid = unsafe { libc::msgget(pid as key_t, 0o666 | libc::IPC_CREAT) };
    if msqid == -1 {
        let msg = Errno::last().desc();
        eprintln!("Failed to open message channel: {}: {}", msqid, msg);
        return Err(1);
    }

    return Ok(msqid);
}

pub struct ResponseChannel {
    response_chan: i32,
}

impl ResponseChannel {
    pub fn receive_message<R: DeserializeOwned>(&self) -> Result<R, i32> {
        let mut message = Message {
            m_type: MESSAGE_TYPE,
            data: Data {
                response_chan: 0,
                response_pid: 0,
                message_type: 0,
                message_length: 0,
                message: [0; MESSAGE_LENGTH],
            },
        };

        let mut buffer = Vec::<u8>::new();

        let mut is_done = false;
        while !is_done {
            let res = unsafe {
                libc::msgrcv(
                    self.response_chan,
                    &mut message as *mut _ as *mut c_void,
                    size_of::<Data>(),
                    MESSAGE_TYPE,
                    0,
                )
            };

            if res == -1 {
                let msg = Errno::last().desc();
                eprintln!(
                    "Failed to receive message from channel: {}: {}",
                    self.response_chan, msg
                );
                return Err(1);
            }

            const MASK: u8 = 0x80;
            is_done = message.data.message_length & MASK == MASK;
            let len = (message.data.message_length & 0x7F) as usize;

            {
                let data = &message.data.message[..len];
                buffer.extend(data);
            }
        }

        let text = match from_utf8(buffer.as_slice()) {
            Ok(s) => s.to_string(),
            Err(e) => {
                eprintln!("Failed to decode response from server: {}", e);
                return Err(1);
            }
        };

        return match serde_json::from_str::<R>(text.as_str()) {
            Ok(r) => Ok(r),
            Err(e) => match serde_json::from_str::<ServerErrorMessage>(text.as_str()) {
                Ok(message) => {
                    eprintln!("{}", message.error);
                    Err(1)
                }
                Err(_) => {
                    eprintln!("Failed to parse response from server: {}", e);
                    Err(1)
                }
            },
        };
    }

    pub fn close(&self) -> Result<(), i32> {
        let res = unsafe { libc::msgctl(self.response_chan, libc::IPC_RMID, null_mut()) };
        if res == -1 {
            let msg = Errno::last().desc();
            eprintln!(
                "Failed to cleanup message channel: {}: {}",
                self.response_chan, msg
            );
            return Err(1);
        }
        return Ok(());
    }
}
