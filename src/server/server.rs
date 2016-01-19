use std::net::SocketAddr;
use std::collections::BTreeMap;
use std::io;
use std::process;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::thread;

use bytes::{Buf, ByteBuf};
use mio::EventLoop;
use rand::{Rng, thread_rng};
use protobuf::{self, Message};
use uuid::Uuid;

use Client;
use constants;
use serialization::{Meta, RangeMeta, Replica, Collection, RetentionPolicy,
                    CollectionType, HaveMetaRes};
use {CliReq, CliRes, Clock, PeerMsg, RealClock, CollectionKind};
use server::{KV, PeerID, Range, SendChannel, State, EventLoopMessage};
use server::traffic_cop::TrafficCop;
use server::storage::kv::upper_bound;

pub struct Server<C: Clock, S: SendChannel> {
    pub clock: Arc<C>,
    pub local_peer_addr: String,
    pub local_cli_addr: String,
    pub id: PeerID,
    pub kv: Arc<KV>,
    pub has_seen_meta: bool,
    pub ranges: BTreeMap<Vec<u8>, Range<C, S>>,
    pub rpc_tx: S,
}

unsafe impl<C: Clock, S: SendChannel> Sync for Server<C, S>{}

impl<C: Clock, S: SendChannel> Server<C, S> {
    pub fn initialize_meta(storage_dir: String,
                           local_peer_addr: String,
                           peers: Vec<String>) {

        warn!("initializing meta with seeds {:?}", peers);

        let replicas = peers.iter().map(|p| {
            let mut replica = Replica::new();
            replica.set_address(p.clone());
            // TODO(tyler) get this some deterministic / non-buggy way?
            replica.set_id(Uuid::new_v4().as_bytes().to_vec());
            replica
        }).collect();

        let mut range = RangeMeta::new();
        range.set_lower(constants::META.to_vec());
        range.set_replicas(protobuf::RepeatedField::from_vec(replicas));

        let mut collection = Collection::new();
        collection.set_prefix(constants::META.to_vec());
        collection.set_name("META".to_string());
        collection.set_field_type(CollectionType::KV);
        collection.set_ranges(protobuf::RepeatedField::from_vec(vec![range]));
        collection.set_replication_factor(3);

        let mut meta = Meta::new();
        meta.set_collections(protobuf::RepeatedField::from_vec(vec![collection]));

        let kv  = KV::new(storage_dir);
        match kv.get_meta() {
            Ok(Some(_m)) => panic!("metadata already exists"),
            Err(e) => panic!(e),
            _ => (),
        }
        kv.persist_meta(&meta).unwrap();
        warn!("metadata initialized, restart db without the --initialize flag now.");
    }

    pub fn populate_meta(&mut self, cached_meta: Meta) -> io::Result<()> {
        let meta_key = b"\x00\x00META";
        let collection = cached_meta.get_collections().first().unwrap();
        assert!(collection.get_prefix() == meta_key);
        let range_meta = collection.get_ranges().first().unwrap();
        assert!(range_meta.get_lower() == meta_key);

        let peers = range_meta.get_replicas()
                                  .iter()
                                  .map(|r| {
                                      let address = r.get_address().to_string();
                                      // tell traffic cop about new peers to stay in touch with
                                      self.rpc_tx.send_msg(EventLoopMessage::AddPeer(address.clone()));
                                      address
                                  })
                                  .collect();

        // Create the range
        let mut range = Range::initial(
            self.id.clone(),
            self.clock.clone(),
            CollectionKind::KV,
            meta_key.to_vec(),
            upper_bound(meta_key).to_vec(),
            self.kv.clone(),
            peers,
            BTreeMap::new(),
            State::Init,
            self.rpc_tx.clone_chan());

        // persist the META metadata to local_meta RANGES key

        // add it to self.ranges
        self.ranges.insert(meta_key.to_vec(), range);

        Ok(())
    }

    pub fn run(storage_dir: String,
               local_peer_addr: String,
               local_cli_addr: String,
               peers: Vec<String>) {
        // All long-running worker threads get a clone of this
        // Sender.  When they exit, they send over it.  If the
        // Receiver ever completes a read, it means something
        // unexpectedly exited.  It's vital that we shut down
        // immediately, so we don't repeat the ZK bug where
        // the heartbeater keeps running while other vital threads
        // have exited, falsely communicating healthiness.
        let (thread_exit_tx, thread_exit_rx) = mpsc::channel();

        // The TrafficCop manages our sockets, sends deserialized
        // messages over the request channel, and receives completed
        // responses over the response channel.
        let (peer_req_tx, peer_req_rx) = mpsc::channel();
        let (cli_req_tx, cli_req_rx) = mpsc::channel();

        let mut tc = TrafficCop::new(local_peer_addr.clone(),
                                     local_cli_addr.clone(),
                                     peers.clone(),
                                     peer_req_tx,
                                     cli_req_tx)
                         .unwrap();

        // A single MIO EventLoop handles our IO
        let mut event_loop = EventLoop::new().unwrap();

        // All RPC's are sent over the event_loop's
        // notification channel.
        let rpc_tx = event_loop.channel();

        // start server periodic tasks
        event_loop.timeout_ms((), thread_rng().gen_range(200, 500)).unwrap();

        // IO event loop thread
        let tex1 = thread_exit_tx.clone();
        thread::Builder::new()
            .name("IO loop".to_string())
            .spawn(move || {
                tc.run_event_loop(event_loop);
                tex1.send(());
            });

        let clock = Arc::new(RealClock);
        let kv = Arc::new(KV::new(storage_dir));

        let server = Arc::new(Mutex::new(Server {
            clock: clock.clone(),
            local_peer_addr: local_peer_addr.clone(),
            local_cli_addr: local_cli_addr,
            id: Uuid::new_v4().to_string(), // TODO(tyler) read from rocksdb
            rpc_tx: rpc_tx,
            kv: kv.clone(),
            ranges: BTreeMap::new(),
            has_seen_meta: false,
        }));

        // peer request handler thread
        let srv1 = server.clone();
        let tex2 = thread_exit_tx.clone();
        thread::Builder::new()
            .name("peer request handler".to_string())
            .spawn(move || {
                for req in peer_req_rx {
                    match srv1.lock() {
                        Ok(mut srv) => srv.handle_peer(req),
                        Err(e) => {
                            error!("{}", e);
                            process::exit(1);
                        }
                    }
                }
                tex2.send(());
            });

        // query peers, only creating meta if:
        //  1. we have fresh META in our cached local meta with ourselves as a replica
        //  1. all seed peers are reachable
        //      (log + retry until they are, because this is a big deal and should sacrifice availability)
        //  1. none of them have heard of META shard before
        //      if any of them have, get it
        let cached_meta = kv.get_meta().unwrap();
        let is_seeding = should_seed(cached_meta.clone(), local_peer_addr.clone(), peers);
        if is_seeding {
            match server.lock() {
                Ok(mut srv) => {
                    warn!("initializing fresh meta range");
                    srv.populate_meta(cached_meta.unwrap());
                },
                Err(e) => {
                    error!("{}", e);
                    process::exit(1);
                }
            }
        } else {
            warn!("waiting on peers to tell us of our schemas, which we will cross-reference with
            what we have on-disk.");
            // TODO(tyler) implement backoff to seeds asking for META
            process::exit(1);
        }
 
        // cli request handler thread
        let srv2 = server.clone();
        let tex3 = thread_exit_tx.clone();
        thread::Builder::new()
            .name("cli request handler".to_string())
            .spawn(move || {
                for req in cli_req_rx {
                    match srv2.lock() {
                        Ok(mut srv) => srv.handle_cli(req),
                        Err(e) => {
                            error!("{}", e);
                            process::exit(1);
                        }
                    }
                }
                tex3.send(());
            });

        // cron thread
        let srv3 = server.clone();
        let tex4 = thread_exit_tx.clone();
        thread::Builder::new()
            .name("server cron".to_string())
            .spawn(move || {
                let mut rng = thread_rng();
                loop {
                    clock.sleep_ms(rng.gen_range(400, 500));
                    match srv3.lock() {
                        Ok(mut srv) => {
                            for (_, range) in srv.ranges.iter_mut() {
                                range.cron()
                            }
                        }
                        Err(e) => {
                            error!("{}", e);
                            process::exit(1);
                        }
                    }
                }
                tex4.send(());
            });

        // this should never receive
        thread_exit_rx.recv();
        let msg = "A worker thread unexpectedly exited! Shutting down.";
        error!("{}", msg);
        panic!("A worker thread unexpectedly exited! Shutting down.");
    }

    pub fn range_for_key<'a>(&self, key: &[u8]) -> Option<&Range<C, S>> {
        let ranges: Vec<&Range<C, S>> = self.ranges
                                             .values()
                                             .filter(|r| {
                                                 &*r.lower <= key &&
                                                 &*r.upper > key
                                             })
                                             .collect();
        if ranges.len() == 1 {
            debug!("routing key request {:?} to range [ {:?} -> {:?} ]", key, ranges[0].lower,
                   ranges[0].upper);
            Some(ranges[0])
        } else {
            warn!("found no range for key {:?}!", key);
            None
        }
    }

    pub fn range_for_key_mut(&mut self,
                             key: &[u8])
                             -> Option<&mut Range<C, S>> {
        let key: Vec<u8> = {
            let mut ranges: Vec<&Vec<u8>> = self.ranges
                                                .iter_mut()
                                                .filter(|&(k, ref r)| {
                                                    &*r.lower <= key &&
                                                    &*r.upper > key
                                                })
                                                .map(|(k, _)| k)
                                                .collect();
            if ranges.len() == 1 {
                debug!("routing key request {:?} to range with lower {:?}", key, ranges[0]);
                ranges[0].clone()
            } else {
                error!("Found none or several matching range keys in \
                        range_for_key_mut!");
                return None;
            }
        };
        self.ranges.get_mut(&*key)
    }

    fn reply(&mut self, elm: EventLoopMessage, res_buf: ByteBuf) {
        match elm {
            EventLoopMessage::Envelope {address, session, msg} => {
                self.rpc_tx.send_msg(EventLoopMessage::Envelope {
                    address: address,
                    session: session,
                    msg: res_buf,
                });
            },
            _ => error!("got reply for non-envelope message!"),
        }
    }

    pub fn handle_peer(&mut self, elm: EventLoopMessage) {
        info!("in handle_peer");
        let msg = match elm.clone() {
            EventLoopMessage::Envelope{msg, ..} => msg,
            _ => {
                error!("received non-envelope message in handle_peer!");
                return;
            },
        };

        let peer_msg: Result<PeerMsg, _> = protobuf::parse_from_bytes(msg.bytes());

        if peer_msg.is_err() {
            // TODO(tyler) this is a hack to let servers handle cli messages because
            // I didn't feel like writing the server client code at 3am at the 32c3.
            let cli_req: CliReq = protobuf::parse_from_bytes(msg.bytes()).unwrap();
            if cli_req.has_have_meta_req() {
                let mut have_meta_res = HaveMetaRes::new();
                have_meta_res.set_has_seen_meta(self.has_seen_meta);

                let mut res = CliRes::new();
                res.set_have_meta_res(have_meta_res);
                res.set_req_id(0);

                self.reply(elm, ByteBuf::from_slice(&*res.write_to_bytes().unwrap()));
            }
        } else {
            self.ranges
                .get_mut(peer_msg.unwrap().get_range_prefix())
                .unwrap()
                .handle_peer(elm);
        }
    }

    fn handle_cli(&mut self, elm: EventLoopMessage) {
        let msg = match elm.clone() {
            EventLoopMessage::Envelope{msg, ..} => msg,
            _ => {
                error!("received non-envelope message in handle_peer!");
                return;
            },
        };

        let cli_req: CliReq = protobuf::parse_from_bytes(msg.bytes())
                                  .unwrap();
        let key = cli_req.get_key();
        let ranges: Vec<Vec<u8>> = self.ranges
                                       .keys()
                                       .cloned()
                                       .filter(|k| key.starts_with(k))
                                       .map(|k| k)
                                       .collect();
        if ranges.len() == 0 {
            // TODO(tyler) reply with range-aware redirect
        }
        self.ranges.get_mut(ranges.last().unwrap()).unwrap().handle_peer(elm);
    }
}

fn we_are_in_local_cached_meta(meta_opt: Option<Meta>, our_addr: String) -> bool {
    match meta_opt {
        None => return false,
        Some(meta) => {
            for collection in meta.get_collections().iter() {
                for range in collection.get_ranges().iter() {
                    for replica in range.get_replicas().iter() {
                        if replica.get_address() == our_addr {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

fn should_seed(cached_meta: Option<Meta>, local_peer_addr: String, peers: Vec<String>) -> bool {
    if we_are_in_local_cached_meta(cached_meta, local_peer_addr.clone()) {
        let peer_addrs = peers.iter().map(|p| p.parse().unwrap()).collect();
        let mut cli = Client::new(peer_addrs, 1);
        loop {
            warn!("trying to query peers to determine suitability of seed");
            match cli.meta_is_available() {
                Ok(false) =>
                    // we reached everything, and didn't get any redirects
                    return true,
                Ok(true) =>
                    // we reached everything, but found some existing meta
                    return false,
                Err(e) => {
                    // we couldn't reach everything
                    error!("couldn't reach all peers to verify that meta has yet to be seeded:
                    {}", e);
                },
            }
            thread::sleep_ms(1000);
        }
    }
    false
}

