use bytes;
use bytes::BufMut;

use futures;
use futures::future;
use futures::future::Future;
use futures::{AsyncSink, Stream, Sink};
use futures::sync::mpsc;

use tokio;
use tokio_codec;

use secp256k1::key::PublicKey;

use lightning::ln::peer_handler;
use lightning::ln::peer_handler::SocketDescriptor as LnSocketTrait;

use std::mem;
use std::sync::{Arc, Mutex};
use std::vec::Vec;
use std::hash::Hash;
use std;

pub struct Connection {
	writer: Option<mpsc::Sender<bytes::Bytes>>,
	event_notify: mpsc::UnboundedSender<()>,
	pending_read: Vec<u8>,
	read_blocker: Option<futures::sync::oneshot::Sender<Result<(), ()>>>,
	read_paused: bool,
	need_disconnect: bool,
	id: u64,
}
impl Connection {
	fn schedule_read(peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>, us: Arc<Mutex<Self>>, reader: futures::stream::SplitStream<tokio_codec::Framed<tokio::net::TcpStream, tokio_codec::BytesCodec>>) {
		let us_ref = us.clone();
		let us_close_ref = us.clone();
		let peer_manager_ref = peer_manager.clone();
		tokio::spawn(reader.for_each(move |b| {
			let pending_read = b.to_vec();
			{
				let mut lock = us_ref.lock().unwrap();
				assert!(lock.pending_read.is_empty());
				if lock.read_paused {
					lock.pending_read = pending_read;
					let (sender, blocker) = futures::sync::oneshot::channel();
					lock.read_blocker = Some(sender);
					return future::Either::A(blocker.then(|_| { Ok(()) }));
				}
			}
			//TODO: There's a race where we don't meet the requirements of disconnect_socket if its
			//called right here, after we release the us_ref lock in the scope above, but before we
			//call read_event!
			match peer_manager.read_event(&mut SocketDescriptor::new(us_ref.clone(), peer_manager.clone()), pending_read) {
				Ok(pause_read) => {
					if pause_read {
						let mut lock = us_ref.lock().unwrap();
						lock.read_paused = true;
					}
				},
				Err(e) => {
					us_ref.lock().unwrap().need_disconnect = false;
					return future::Either::B(future::result(Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e))));
				}
			}

			us_ref.lock().unwrap().event_notify.unbounded_send(()).unwrap();

			future::Either::B(future::result(Ok(())))
		}).then(move |_| {
			if us_close_ref.lock().unwrap().need_disconnect {
				peer_manager_ref.disconnect_event(&SocketDescriptor::new(us_close_ref, peer_manager_ref.clone()));
				println!("Peer disconnected!");
			} else {
				println!("We disconnected peer!");
			}
			Ok(())
		}));
	}

	fn new(event_notify: mpsc::UnboundedSender<()>, stream: tokio::net::TcpStream, id: u64) -> (futures::stream::SplitStream<tokio_codec::Framed<tokio::net::TcpStream, tokio_codec::BytesCodec>>, Arc<Mutex<Self>>) {
		let (writer, reader) = tokio_codec::Framed::new(stream, tokio_codec::BytesCodec::new()).split();
		let (send_sink, send_stream) = mpsc::channel(3);
		tokio::spawn(writer.send_all(send_stream.map_err(|_| -> std::io::Error {
			unreachable!();
		})).then(|_| {
			future::result(Ok(()))
		}));
		let us = Arc::new(Mutex::new(Self { writer: Some(send_sink), event_notify, pending_read: Vec::new(), read_blocker: None, read_paused: false, need_disconnect: true, id }));

		(reader, us)
	}

	pub fn setup_inbound(peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>, event_notify: mpsc::UnboundedSender<()>, stream: tokio::net::TcpStream, id: u64) {
		let (reader, us) = Self::new(event_notify, stream, id);

		if let Ok(_) = peer_manager.new_inbound_connection(SocketDescriptor::new(us.clone(), peer_manager.clone())) {
			Self::schedule_read(peer_manager, us, reader);
		}
	}

	pub fn setup_outbound(peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>, event_notify: mpsc::UnboundedSender<()>, their_node_id: PublicKey, stream: tokio::net::TcpStream, id: u64) {
		let (reader, us) = Self::new(event_notify, stream, id);

		if let Ok(initial_send) = peer_manager.new_outbound_connection(their_node_id, SocketDescriptor::new(us.clone(), peer_manager.clone())) {
			if SocketDescriptor::new(us.clone(), peer_manager.clone()).send_data(&initial_send, 0, true) == initial_send.len() {
				Self::schedule_read(peer_manager, us, reader);
			} else {
				println!("Failed to write first full message to socket!");
			}
		}
	}
}

#[derive(Clone)]
pub struct SocketDescriptor {
	conn: Arc<Mutex<Connection>>,
	id: u64,
	peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>,
}
impl SocketDescriptor {
	fn new(conn: Arc<Mutex<Connection>>, peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>) -> Self {
		let id = conn.lock().unwrap().id;
		Self { conn, id, peer_manager }
	}
}
impl peer_handler::SocketDescriptor for SocketDescriptor {
	fn send_data(&mut self, data: &Vec<u8>, write_offset: usize, resume_read: bool) -> usize {
		macro_rules! schedule_read {
			($us_ref: expr) => {
				tokio::spawn(future::lazy(move || -> Result<(), ()> {
					let mut read_data = Vec::new();
					{
						let mut us = $us_ref.conn.lock().unwrap();
						mem::swap(&mut read_data, &mut us.pending_read);
					}
					if !read_data.is_empty() {
						let mut us_clone = $us_ref.clone();
						match $us_ref.peer_manager.read_event(&mut us_clone, read_data) {
							Ok(pause_read) => {
								if pause_read { return Ok(()); }
							},
							Err(_) => {
								//TODO: Not actually sure how to do this
								return Ok(());
							}
						}
					}
					let mut us = $us_ref.conn.lock().unwrap();
					if let Some(sender) = us.read_blocker.take() {
						sender.send(Ok(())).unwrap();
					}
					us.read_paused = false;
					us.event_notify.unbounded_send(()).unwrap();
					Ok(())
				}));
			}
		}

		let mut us = self.conn.lock().unwrap();
		if resume_read {
			let us_ref = self.clone();
			schedule_read!(us_ref);
		}
		if data.len() == write_offset { return 0; }
		if us.writer.is_none() {
			us.read_paused = true;
			return 0;
		}

		let mut bytes = bytes::BytesMut::with_capacity(data.len() - write_offset);
		bytes.put(&data[write_offset..]);
		let write_res = us.writer.as_mut().unwrap().start_send(bytes.freeze());
		match write_res {
			Ok(res) => {
				match res {
					AsyncSink::Ready => {
						data.len() - write_offset
					},
					AsyncSink::NotReady(_) => {
						us.read_paused = true;
						let us_ref = self.clone();
						tokio::spawn(us.writer.take().unwrap().flush().then(move |writer_res| -> Result<(), ()> {
							if let Ok(writer) = writer_res {
								{
									let mut us = us_ref.conn.lock().unwrap();
									us.writer = Some(writer);
								}
								schedule_read!(us_ref);
							} // we'll fire the disconnect event on the socket reader end
							Ok(())
						}));
						0
					}
				}
			},
			Err(_) => {
				// We'll fire the disconnected event on the socket reader end
				0
			},
		}
	}

	fn disconnect_socket(&mut self) {
		let mut us = self.conn.lock().unwrap();
		us.need_disconnect = true;
		us.read_paused = true;
	}
}
impl Eq for SocketDescriptor {}
impl PartialEq for SocketDescriptor {
	fn eq(&self, o: &Self) -> bool {
		self.id == o.id
	}
}
impl Hash for SocketDescriptor {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.id.hash(state);
	}
}

