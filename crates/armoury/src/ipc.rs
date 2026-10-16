//! Named-pipe server. One accept thread; one handler thread per connection.
//! Events flow to the main loop via a crossbeam channel.

use armoury_core::protocol::{read_msg, write_msg, Request, Response};
use crossbeam_channel::Sender;
use interprocess::local_socket::{prelude::*, Listener, Stream};
use log::{info, warn};
use std::io::BufReader;
use std::thread;

pub enum PipeEvent {
    Connected { id: u64, push: Sender<Response> },
    Request { id: u64, req: Request },
    Disconnected { id: u64 },
}

/// Spawn the accept loop on a dedicated thread.
pub fn start(listener: Listener, tx: Sender<PipeEvent>) {
    thread::spawn(move || {
        let mut next_id: u64 = 1;
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    let id = next_id;
                    next_id += 1;
                    let tx = tx.clone();
                    thread::spawn(move || handle_conn(stream, id, tx));
                }
                Err(e) => warn!("pipe accept error: {e}"),
            }
        }
    });
}

fn handle_conn(stream: Stream, id: u64, tx: Sender<PipeEvent>) {
    let (recv_half, send_half) = stream.split();

    // Writer thread: daemon -> client.
    // Unbounded so a slow/stuck client can never block the daemon main loop.
    let (push_tx, push_rx) = crossbeam_channel::unbounded::<Response>();
    let writer = thread::spawn(move || {
        let mut w = send_half;
        while let Ok(msg) = push_rx.recv() {
            if write_msg(&mut w, &msg).is_err() {
                break;
            }
        }
    });

    let _ = tx.send(PipeEvent::Connected {
        id,
        push: push_tx.clone(),
    });

    // Reader loop (this thread): client -> daemon.
    let mut reader = BufReader::new(recv_half);
    let mut line = String::new();
    loop {
        match read_msg::<_, Request>(&mut reader, &mut line) {
            Ok(Some(req)) => {
                if tx.send(PipeEvent::Request { id, req }).is_err() {
                    break;
                }
            }
            Ok(None) => break, // clean EOF: client closed the pipe
            Err(e) => {
                warn!("pipe client {id}: bad message ({e}); dropping connection");
                break;
            }
        }
    }

    let _ = tx.send(PipeEvent::Disconnected { id });
    drop(push_tx);
    let _ = writer.join();
    info!("pipe client {id} handler finished");
}
