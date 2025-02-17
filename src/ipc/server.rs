use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::rc::Rc;
use std::{env, io, process};

use anyhow::Context;
use calloop::io::Async;
use directories::BaseDirs;
use futures_util::io::{AsyncReadExt, BufReader};
use futures_util::{AsyncBufReadExt, AsyncWriteExt};
use niri_ipc::{Request, Response};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::reexports::rustix::fs::unlink;

use crate::niri::State;

pub struct IpcServer {
    pub socket_path: PathBuf,
}

struct ClientCtx {
    event_loop: LoopHandle<'static, State>,
    ipc_outputs: Rc<RefCell<HashMap<String, niri_ipc::Output>>>,
}

impl IpcServer {
    pub fn start(
        event_loop: &LoopHandle<'static, State>,
        wayland_socket_name: &str,
    ) -> anyhow::Result<Self> {
        let _span = tracy_client::span!("Ipc::start");

        let socket_name = format!("niri.{wayland_socket_name}.{}.sock", process::id());
        let mut socket_path = socket_dir();
        socket_path.push(socket_name);

        let listener = UnixListener::bind(&socket_path).context("error binding socket")?;
        listener
            .set_nonblocking(true)
            .context("error setting socket to non-blocking")?;

        let source = Generic::new(listener, Interest::READ, Mode::Level);
        event_loop
            .insert_source(source, |_, socket, state| {
                match socket.accept() {
                    Ok((stream, _)) => on_new_ipc_client(state, stream),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                    Err(e) => return Err(e),
                }

                Ok(PostAction::Continue)
            })
            .unwrap();

        Ok(Self { socket_path })
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = unlink(&self.socket_path);
    }
}

fn socket_dir() -> PathBuf {
    BaseDirs::new()
        .as_ref()
        .and_then(|x| x.runtime_dir())
        .map(|x| x.to_owned())
        .unwrap_or_else(env::temp_dir)
}

fn on_new_ipc_client(state: &mut State, stream: UnixStream) {
    let _span = tracy_client::span!("on_new_ipc_client");
    trace!("new IPC client connected");

    let stream = match state.niri.event_loop.adapt_io(stream) {
        Ok(stream) => stream,
        Err(err) => {
            warn!("error making IPC stream async: {err:?}");
            return;
        }
    };

    let ctx = ClientCtx {
        event_loop: state.niri.event_loop.clone(),
        ipc_outputs: state.backend.ipc_outputs(),
    };

    let future = async move {
        if let Err(err) = handle_client(ctx, stream).await {
            warn!("error handling IPC client: {err:?}");
        }
    };
    if let Err(err) = state.niri.scheduler.schedule(future) {
        warn!("error scheduling IPC stream future: {err:?}");
    }
}

async fn handle_client(ctx: ClientCtx, stream: Async<'_, UnixStream>) -> anyhow::Result<()> {
    let (read, mut write) = stream.split();
    let mut buf = String::new();

    // Read a single line to allow extensibility in the future to keep reading.
    BufReader::new(read)
        .read_line(&mut buf)
        .await
        .context("error reading request")?;

    let request: Request = serde_json::from_str(&buf).context("error parsing request")?;

    let response = match request {
        Request::Outputs => {
            let ipc_outputs = ctx.ipc_outputs.borrow().clone();
            Response::Outputs(ipc_outputs)
        }
        Request::Action(action) => {
            let action = niri_config::Action::from(action);
            ctx.event_loop.insert_idle(move |state| {
                state.do_action(action);
            });
            return Ok(());
        }
    };

    let buf = serde_json::to_vec(&response).context("error formatting response")?;
    write
        .write_all(&buf)
        .await
        .context("error writing response")?;

    Ok(())
}
