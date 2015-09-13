use std::collections::{BTreeMap};
use std::io::{Error, ErrorKind};
use std::io;
use std::net::SocketAddr;
use std::ops::{Add, Sub};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{self, Sender, Receiver};
use std::thread;
use std::usize;

use bytes::{alloc, Buf, ByteBuf, MutByteBuf, SliceBuf};
use mio;
use mio::{EventLoop, EventSet, PollOpt, Handler, Token, TryWrite, TryRead};
use mio::tcp::{TcpListener, TcpStream, TcpSocket};
use mio::util::Slab;
use rand::{Rng, thread_rng};
use rocksdb::{DB, Writable};
use rocksdb::Options as RocksDBOptions;
use protobuf;
use protobuf::Message;
use time;

use ::{CliReq, CliRes, GetReq, GetRes, PeerMsg,
    RedirectRes, SetReq, SetRes, VoteReq, VoteRes,
    Append, AppendRes, VersionedKV};
use server::{Envelope, State, LEADER_REFRESH, LEADER_DURATION, PEER_BROADCAST};
use server::{AckedLog, LogEntry, Learn};
use server::traffic_cop::TrafficCop;

pub struct Server {
    peer_port: u16,
    cli_port: u16,
    id: u64,
    peers: Vec<String>,
    res_tx: mio::Sender<Envelope>,
    bcast_epoch: u64,
    max_txid: u64,
    highest_term: u64,
    last_tx_term: u64,
    state: State,
    db: DB,
    rep_log: AckedLog<Envelope, Sender<u64>>,
    learned_rx: Receiver<u64>,
}

impl Server {

    pub fn run(
        peer_port: u16,
        cli_port: u16,
        storage_dir: String,
        peers: Vec<String>
    ) {
        let mut opts = RocksDBOptions::new();
        let memtable_budget = 1024;
        opts.optimize_level_style_compaction(memtable_budget);
        opts.create_if_missing(true);
        let db = match DB::open_cf(&opts, &storage_dir,
                                   &["storage", "local_meta"]) {
            Ok(db) => db,
            Err(_) => {
                info!("Attempting to initialize data directory at {}",
                      storage_dir);
                match DB::open(&opts, &storage_dir) {
                    Ok(mut db) => {
                        db.create_cf(
                            "storage", &RocksDBOptions::new()).unwrap();
                        db.create_cf(
                            "local_meta", &RocksDBOptions::new()).unwrap();
                        db
                    },
                    Err(e) => {
                        error!("failed to create database at {}", storage_dir);
                        error!("{}", e);
                        panic!(e);
                    },
                }
            }
        };

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

        let mut tc = TrafficCop::new(
            peer_port,
            cli_port,
            peers.clone(),
            peer_req_tx,
            cli_req_tx,
        ).unwrap();

        let mut event_loop = EventLoop::new().unwrap();
        let res_tx = event_loop.channel();

        // start server periodic tasks
        let mut rng = thread_rng();
        event_loop.timeout_ms((), rng.gen_range(200,500)).unwrap();

        // io event loop thread
        let tex1 = thread_exit_tx.clone();
        thread::Builder::new()
            .name("io loop".to_string())
            .spawn( move || {

            tc.run_event_loop(event_loop);
            tex1.send(());
        });

        let (ack_tx, ack_rx) = mpsc::channel();
        let server = Arc::new(Mutex::new(Server {
            peer_port: peer_port,
            cli_port: cli_port,
            id: peer_port as u64 + cli_port as u64
                + time::now().to_timespec().nsec as u64,
            res_tx: res_tx,
            bcast_epoch: 0,
            max_txid: 0, // TODO(tyler) read from rocksdb
            highest_term: 0, // TODO(tyler) read from rocksdb
            last_tx_term: 0, // TODO(tyler) read from rocksdb
            state: State::Init,
            db: db,
            rep_log: AckedLog {
                pending: BTreeMap::new(),
                committed: vec![],
                learner: ack_tx,
                quorum: peers.len() / 2 + 1,
                last_committed_txid: 0, // TODO(tyler) read from rocksdb
            },
            peers: peers,
            learned_rx: ack_rx,
        }));

        // peer request handler thread
        let srv1 = server.clone();
        let tex2 = thread_exit_tx.clone();
        thread::Builder::new()
            .name("peer request handler".to_string())
            .spawn( move || {

            for req in peer_req_rx {
                srv1.lock().unwrap().handle_peer(req);
            }
            tex2.send(());
        });

        // cli request handler thread
        let srv2 = server.clone();
        let tex3 = thread_exit_tx.clone();
        thread::Builder::new()
            .name("cli request handler".to_string())
            .spawn( move || {

            for req in cli_req_rx {
                srv2.lock().unwrap().handle_cli(req);
            }
            tex3.send(());
        });

        // cron thread
        let srv3 = server.clone();
        let tex4 = thread_exit_tx.clone();
        thread::Builder::new()
            .name("server cron".to_string())
            .spawn( move || {

            let mut rng = thread_rng();
            loop {
                thread::sleep_ms(rng.gen_range(400,500));
                srv3.lock().unwrap().cron();
            }
            tex4.send(());
        });

        // this should never receive
        thread_exit_rx.recv();
        let msg = "A worker thread unexpectedly exited! Shutting down.";
        error!("{}", msg);
        panic!("A worker thread unexpectedly exited! Shutting down.");
    }

    fn handle_vote_res(
        &mut self,
        env: Envelope,
        peer_id: u64,
        vote_res: &VoteRes
    ) {
        debug!("got response for vote request");
        let term = self.state.term();

        if term.is_none() || vote_res.get_term() != term.unwrap() {
            // got response for an term that is not valid
            return
        }

        // Reset if we get any nacks as a candidate.
        // This is a difference from Raft, where any node can dethrone
        // an otherwise healthy leader with a higher term.  We will give
        // up on our own if we don't get a majority of unique votes
        // by the time our leader lease expires.  This protects us against
        // a single partially partitioned node from livelocking our cluster.
        if self.state.valid_candidate() && !vote_res.get_success() {
            // TODO(tyler) set term in rocksdb
            if vote_res.get_term() > self.highest_term {
                self.highest_term = vote_res.get_term();
            }
            self.state = State::Init;
        } else if self.state.valid_candidate() {
            // we're currently a candidate, so see if we can ascend to
            // leader or if we need to give up
            self.state = match self.state {
                State::Candidate{
                    term: term,
                    until: until,
                    need: need,
                    have: ref have,
                } => {
                    let mut new_have = have.clone();
                    if !new_have.contains(&env.tok) &&
                        vote_res.get_term() == term {
                        new_have.push(env.tok);
                    }
                    if new_have.len() >= need as usize {
                        // we've ascended to leader!
                        info!("{} transitioning to leader state", self.id);
                        new_have = vec![];
                        let state = State::Leader{
                            term: term,
                            until: until, // don't extend until
                            need: need,
                            have: new_have,
                        };
                        info!("{:?}", state);
                        Some(state)
                    } else {
                        // we still need more votes
                        Some(State::Candidate{
                            term: term,
                            until: until,
                            need: need,
                            have: new_have,
                        })
                    }
                },
                _ => None,
            }.unwrap();

        } else if self.state.is_leader() &&
            // see if we have a majority of peers, required for extension
            self.state.valid_leader() &&
            vote_res.get_success() {

            self.state = match self.state {
                State::Leader{
                    term: term,
                    until: until,
                    need: need,
                    have: ref have
                } => {
                    let mut new_until = until;
                    let mut new_have = have.clone();
                    if !new_have.contains(&env.tok) &&
                        vote_res.get_term() == term {
                        new_have.push(env.tok);
                    }
                    if new_have.len() >= need as usize {
                        debug!("{} leadership extended", self.id);
                        new_have = vec![];
                        new_until = time::now()
                            .to_timespec()
                            .add(*LEADER_DURATION);
                    }
                    Some(State::Leader{
                        term: term,
                        until: new_until,
                        need: need,
                        have: new_have,
                    })
                },
                _ => None,
            }.unwrap()
        } else if !vote_res.get_success() {
            warn!("{} received vote nack from {}", self.id, peer_id);
        } else {
            // this can happen if a vote res is received by a follower
            error!("got vote response, but we can't handle it");
            error!("valid leader: {}", self.state.valid_leader());
            error!("is leader: {}", self.state.is_leader());
            error!("valid candidate: {}", self.state.valid_candidate());
            error!("is candidate: {}", self.state.is_candidate());
            error!("res term: {}", vote_res.get_term());
            error!("our term: {}", self.state.term().unwrap());
        }
    }

    fn handle_vote_req(
        &mut self,
        env: Envelope,
        peer_id: u64,
        vote_req: &VoteReq
    ) {
        let mut res = PeerMsg::new();
        res.set_srvid(self.id);
        let mut vote_res = VoteRes::new();
        vote_res.set_term(vote_req.get_term());

        if peer_id == self.id {
            // if we are this node (broadcast is naive) then all is well
            // reply to self but don't change to follower
            vote_res.set_success(true);
        } else if self.state.valid_leader() &&
            !self.state.following(peer_id) {
            // if we're already following a different node, reject

            warn!("got unwanted vote req from {}", peer_id);
            // communicate to the source what our term is so they
            // can quickly get followers when we're dead.
            vote_res.set_term(self.state.term().unwrap());
            vote_res.set_success(false);
        } else if self.state.following(peer_id) {
            // if we're already following this node, keed doing so
            debug!("{} extending followership of {}", self.id, peer_id);
            self.state = match self.state {
                State::Follower{
                    term: term,
                    id: id,
                    leader_addr: leader_addr,
                    until: _,
                    tok: tok,
                } => Some(State::Follower {
                    term: term,
                    id: id,
                    leader_addr: leader_addr,
                    until: time::now().to_timespec().add(*LEADER_DURATION),
                    tok: tok,
                }),
                _ => None,
            }.unwrap();
            vote_res.set_success(true);
        } else if !self.state.valid_leader() &&
            vote_req.get_term() >= self.last_tx_term &&
            ((vote_req.get_maxtxid() >= self.max_txid &&
            vote_req.get_last_tx_term() == self.last_tx_term) ||
            (vote_req.get_last_tx_term() > self.last_tx_term)) {
            // accept this node as the leader if it has a higher term than
            // we've ever seen and either one of the following conditions:
            // 1. it has a higher previous max successful tx term
            // 2. it has the same previous max successful tx term and at
            //    least as many entries as we do for it.
            //
            // These conditions guarantee that we don't lose acked writes
            // as long as a majority of our previous nodes stay alive.

            self.highest_term = vote_req.get_term();
            info!("new leader {}", peer_id);
            self.state = State::Follower {
                id: peer_id,
                term: vote_req.get_term(),
                tok: env.tok,
                leader_addr: env.address.unwrap(),
                until: time::now().to_timespec().add(*LEADER_DURATION),
            };
            info!("{:?}", self.state);
            vote_res.set_success(true);
        } else {
            match self.state.term() {
                Some(term) =>
                    vote_res.set_term(term),
                None => (),
            }

            vote_res.set_success(false);
        }
        res.set_vote_res(vote_res);
        self.reply(env, ByteBuf::from_slice(
            &*res.write_to_bytes().unwrap().into_boxed_slice()
        ));
    }

    fn handle_append(
        &mut self,
        env: Envelope,
        peer_id: u64,
        append: &Append
    ) {
        let mut res = PeerMsg::new();
        res.set_srvid(self.id);

        // verify that we are following this node
        if self.state.is_following(peer_id) {
            
        }
    }

    fn handle_append_res(
        &mut self,
        env: Envelope,
        peer_id: u64,
        append_res: &AppendRes
    ) {
        let mut res = PeerMsg::new();
        res.set_srvid(self.id);
        // verify that we are leading
        //
    }

    fn handle_peer(&mut self, env: Envelope) {
        let peer_msg: PeerMsg =
            protobuf::parse_from_bytes(env.msg.bytes()).unwrap();
        let peer_id = peer_msg.get_srvid();

        if peer_msg.has_vote_res() {
            self.handle_vote_res(env, peer_id, peer_msg.get_vote_res());
        } else if peer_msg.has_vote_req() {
            self.handle_vote_req(env, peer_id, peer_msg.get_vote_req());
        } else if peer_msg.has_append() {
            self.handle_append(env, peer_id, peer_msg.get_append());
        } else if peer_msg.has_append_res() {
            self.handle_append_res(env, peer_id, peer_msg.get_append_res());
        } else {
            error!("got unhandled peer message! {:?}", peer_msg);
        }
    }

    fn handle_cli(&mut self, req: Envelope) {
        debug!("got cli request!");
        let cli_req: CliReq =
            protobuf::parse_from_bytes(req.msg.bytes()).unwrap();
        let mut res = CliRes::new();
        if !self.state.is_leader() {
            // If we aren't the leader, we must return some sort of
            // a RedirectRes instead of a response.
            let mut redirect_res = RedirectRes::new();
            redirect_res.set_msgid(req.id);
            // If we're a follower, a leader has been elected, so
            // sets the return address.
            if self.state.is_follower() {
                let leader_address = match self.state {
                    State::Follower{
                        term: _,
                        id: _,
                        leader_addr: leader_addr,
                        until: _,
                        tok: _,
                    } => Some(leader_addr),
                    _ => None,
                }.unwrap();
                redirect_res.set_success(true);
                redirect_res.set_address(format!("{:?}", leader_address));
            } else {
                redirect_res.set_success(false);
                redirect_res
                    .set_err("No leader has been elected yet".to_string());
            }
            res.set_redirect(redirect_res);
        } else if cli_req.has_get() {
            let get_req = cli_req.get_get();
            let mut get_res = GetRes::new();
            self.db.get(get_req.get_key())
                .map( |value| {
                    get_res.set_success(true);
                    get_res.set_value((*value).to_vec());
                })
                .on_absent( || {
                    get_res.set_success(false);
                    get_res.set_err("Key not found".to_string())
                })
                .on_error( |e| {
                    error!("Operational problem encountered: {}", e);
                    get_res.set_success(false);
                    get_res.set_err(
                        "Operational problem encountered".to_string());
                });
            get_res.set_txid(self.max_txid);
            res.set_get(get_res);
        } else if cli_req.has_set() {
            let set_req = cli_req.get_set();
            let mut set_res = SetRes::new();
            match self.db.put(set_req.get_key(), set_req.get_value()) {
                Ok(_) => set_res.set_success(true),
                Err(e) => {
                    error!(
                        "Operational problem encountered: {}", e);
                    set_res.set_success(false);
                    set_res.set_err(
                        "Operational problem encountered".to_string());
                }
            }
            set_res.set_txid(self.max_txid);
            res.set_set(set_res);
        }

        self.reply(req, ByteBuf::from_slice(
            &*res.write_to_bytes().unwrap().into_boxed_slice()
        ));
    }

    fn cron(&mut self) {
        debug!("{} state: {:?}", self.id, self.state);
        // become candidate if we need to
        if !self.state.valid_leader() && !self.state.valid_candidate() {
            info!("{} transitioning to candidate state", self.id);
            self.highest_term += 1;
            self.state = State::Candidate {
                term: self.highest_term,
                until: time::now().to_timespec().add(*LEADER_DURATION),
                need: (self.peers.len() / 2 + 1) as u8,
                have: vec![],
            };
            info!("{:?}", self.state);
        }

        // request or extend leadership
        if self.state.should_extend_leadership() ||
            self.state.valid_candidate() {

            debug!("broadcasting VoteReq");
            let mut req = PeerMsg::new();
            req.set_srvid(self.id);
            let mut vote_req = VoteReq::new();
            vote_req.set_term(self.state.term().unwrap());
            vote_req.set_maxtxid(self.max_txid);
            req.set_vote_req(vote_req);
            self.peer_broadcast(
                ByteBuf::from_slice(
                    &*req.write_to_bytes().unwrap().into_boxed_slice()
                )
            );
        }

        // heartbeat
        if self.state.is_leader() {
            let mut vkv = VersionedKV::new();
            vkv.set_txid(self.new_txid());
            vkv.set_term(self.state.term().unwrap());
            vkv.set_key(b"heartbeat".to_vec());
            vkv.set_value(format!("{}", time::now().to_timespec().sec)
                          .as_bytes()
                          .to_vec());
            self.replicate(vec![vkv]);
        }
    }

    fn new_txid(&mut self) -> u64 {
        self.max_txid += 1;
        self.max_txid
    }

    fn reply(&mut self, req: Envelope, res_buf: ByteBuf) {
        self.res_tx.send(Envelope {
            id: req.id,
            address: req.address,
            tok: req.tok,
            msg: res_buf,
        });
    }

    fn peer_broadcast(&mut self, msg: ByteBuf) {
        self.res_tx.send(Envelope {
            id: self.bcast_epoch,
            address: None,
            tok: PEER_BROADCAST,
            msg: msg,
        });
        self.bcast_epoch += 1;
    }

    fn replicate(&mut self, vkvs: Vec<VersionedKV>) {
        // TODO(tyler) add to replication machine
        let mut append = Append::new();
        append.set_batch_id(self.new_txid()); 
        append.set_from_txid(self.new_txid());
        append.set_from_term(self.state.term().unwrap());
        append.set_batch(protobuf::RepeatedField::from_vec(vec![]));

        let mut peer_msg = PeerMsg::new();
        peer_msg.set_srvid(self.id);
        peer_msg.set_append(append);

        self.peer_broadcast(
            ByteBuf::from_slice(
                &*peer_msg.write_to_bytes().unwrap().into_boxed_slice()
            )
        );
    }
}

impl Learn<Envelope> for Sender<u64> {
    fn learn(&mut self, env: &LogEntry<Envelope>) {
        self.send(env.txid);
    }
}
