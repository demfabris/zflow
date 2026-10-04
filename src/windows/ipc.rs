use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use tokio::{
    net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions},
    sync::{mpsc, oneshot},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        },
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Status,
    Nearby {
        address: String,
    },
    Trust {
        key: String,
    },
    Forget {
        peer: String,
    },
    Sharing {
        enabled: bool,
    },
    Clipboard {
        enabled: bool,
    },
    PauseAtEdges {
        enabled: bool,
    },
    Keyboard {
        peer: String,
        mode: crate::core::KeyboardMode,
    },
    ReverseScroll {
        peer: String,
        enabled: bool,
    },
    Arrange {
        layout: crate::app::layout_model::Layout,
        version: u64,
    },
    Activate {
        peer: String,
    },
    Local,
    Quit,
}
pub struct Request {
    pub command: Command,
    pub reply: oneshot::Sender<Value>,
}

pub fn pipe_name(path: &Path) -> Result<String> {
    let path = std::path::absolute(path)?;
    let hash = crate::identity::encode_hex(&Sha256::digest(
        path.to_string_lossy().to_lowercase().as_bytes(),
    ));
    Ok(format!(r"\\.\pipe\zflow-{}", &hash[..24]))
}

pub(super) fn user_sid() -> Result<String> {
    unsafe {
        let mut token = std::ptr::null_mut();
        ensure!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) != 0,
            "Cannot read user token"
        );
        let mut size = 0;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut size);
        // usize storage ensures TOKEN_USER is correctly aligned.
        let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        );
        CloseHandle(token);
        ensure!(ok != 0, "Cannot read user SID");
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut string = std::ptr::null_mut();
        ensure!(
            ConvertSidToStringSidW(user.User.Sid, &mut string) != 0,
            "Cannot format user SID"
        );
        let mut len = 0;
        while *string.add(len) != 0 {
            len += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(string, len));
        LocalFree(string.cast());
        Ok(sid)
    }
}

fn server(name: &str, first: bool) -> Result<NamedPipeServer> {
    let sddl: Vec<u16> = format!("D:P(A;;GA;;;{})", user_sid()?)
        .encode_utf16()
        .chain([0])
        .collect();
    unsafe {
        let mut descriptor = std::ptr::null_mut();
        ensure!(
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut()
            ) != 0,
            "Cannot protect local control pipe"
        );
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let result = ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            );
        LocalFree(descriptor);
        result.context("Cannot listen for the app. Is zflow already running?")
    }
}

pub fn listen(path: &Path) -> Result<(mpsc::Receiver<Request>, tokio::task::JoinHandle<()>)> {
    let name = pipe_name(path)?;
    let mut pipe = server(&name, true)?;
    let (tx, rx) = mpsc::channel::<Request>(32);
    let task = tokio::spawn(async move {
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(16));
        loop {
            if pipe.connect().await.is_err() {
                break;
            }
            let next = match server(&name, false) {
                Ok(p) => p,
                Err(_) => break,
            };
            let mut connected = std::mem::replace(&mut pipe, next);
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                continue;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let _ = tokio::time::timeout(Duration::from_secs(15), async {
                    let command = crate::control::read_message(&mut connected).await?;
                    let (reply, receive) = oneshot::channel();
                    tx.send(Request { command, reply })
                        .await
                        .map_err(std::io::Error::other)?;
                    let response = receive.await.map_err(std::io::Error::other)?;
                    crate::control::write_message(&mut connected, &response).await?;
                    Ok::<_, anyhow::Error>(())
                })
                .await;
            });
        }
    });
    Ok((rx, task))
}

pub async fn request(path: &Path, command: Command) -> Result<Value> {
    let name = pipe_name(path)?;
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut pipe = loop {
            match ClientOptions::new().open(&name) {
                Ok(pipe) => break pipe,
                Err(e) if e.raw_os_error() == Some(231) => {
                    tokio::time::sleep(Duration::from_millis(25)).await
                }
                Err(e) => {
                    return Err(e)
                        .context("zflow is not running. Open the Windows app or run `zflow run`");
                }
            }
        };
        crate::control::write_message(&mut pipe, &command).await?;
        let value: Value = crate::control::read_message(&mut pipe).await?;
        if let Some(error) = value.get("error").and_then(Value::as_str) {
            anyhow::bail!("{error}");
        }
        Ok(value)
    })
    .await
    .context("Windows engine did not answer")?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn pipe_is_exclusive_and_round_trips_framed_commands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (mut rx, task) = listen(&path).unwrap();
        assert!(listen(&path).is_err());
        let answer = tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert!(matches!(req.command, Command::Status));
            req.reply.send(serde_json::json!({"ok":true})).unwrap();
        });
        assert_eq!(request(&path, Command::Status).await.unwrap()["ok"], true);
        answer.await.unwrap();
        task.abort();
    }
}
