// Copyright 2016 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use cache::{
    Storage,
    storage_from_environment,
};
use compiler::{
    CacheControl,
    Compiler,
    CompilerArguments,
    CompileResult,
    MissType,
    ParsedArguments,
    get_compiler_info,
};
use filetime::FileTime;
use futures::future;
use futures::sync::mpsc;
use futures::task::{self, Task};
use futures::{Stream, Sink, Async, AsyncSink, Poll, StartSend, Future};
use futures_cpupool::CpuPool;
use mock_command::{
    CommandCreatorSync,
    ProcessCommandCreator,
};
use protobuf::{
    self,
    ProtobufError,
    RepeatedField,
    parse_length_delimited_from_bytes,
};
use protocol::{
    ClientRequest,
    CacheStats,
    CacheStatistic,
    Compile,
    CompileFinished,
    CompileStarted,
    ServerResponse,
    ShuttingDown,
    UnhandledCompile,
    UnknownCommand,
};
use std::collections::HashMap;
use std::cell::RefCell;
use std::env;
use std::ffi::OsString;
use std::fs::metadata;
use std::io::{self, ErrorKind, Write};
use std::marker;
use std::net::{SocketAddr, SocketAddrV4, Ipv4Addr};
use std::process::Output;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;
use tokio_core::reactor::{Handle, Core, Timeout};
use tokio_core::io::{Codec, EasyBuf, Io, Framed};
use tokio_core::net::TcpListener;
use tokio_proto::BindServer;
use tokio_proto::streaming::pipeline::{Frame, ServerProto, Transport};
use tokio_proto::streaming::{Body, Message};
use tokio_service::Service;

/// If the server is idle for this many milliseconds, shut down.
const DEFAULT_IDLE_TIMEOUT: u64 = 600_000;

fn notify_server_startup_internal<W: Write>(mut w: W, success: bool) -> io::Result<()> {
    let data = [ if success { 0 } else { 1 }; 1];
    try!(w.write_all(&data));
    Ok(())
}

#[cfg(unix)]
fn notify_server_startup(name: &Option<OsString>, success: bool) -> io::Result<()> {
    use std::os::unix::net::UnixStream;
    let name = match *name {
        Some(ref s) => s,
        None => return Ok(()),
    };
    debug!("notify_server_startup(success: {})", success);
    let stream = try!(UnixStream::connect(name));
    notify_server_startup_internal(stream, success)
}

#[cfg(windows)]
fn notify_server_startup(name: &Option<OsString>, success: bool) -> io::Result<()> {
    use named_pipe::PipeClient;
    let name = match *name {
        Some(ref s) => s,
        None => return Ok(()),
    };
    let pipe = try!(PipeClient::connect(name));
    notify_server_startup_internal(pipe, success)
}

/// Start an sccache server, listening on `port`.
///
/// Spins an event loop handling client connections until a client
/// requests a shutdown.
pub fn start_server(port: u16) -> io::Result<()> {
    let res = SccacheServer::<ProcessCommandCreator>::new(port);
    let notify = env::var_os("SCCACHE_STARTUP_NOTIFY");
    match res {
        Ok(srv) => {
            try!(notify_server_startup(&notify, true));
            srv.run(future::empty::<(), ()>())
        }
        Err(e) => {
            try!(notify_server_startup(&notify, false));
            Err(e)
        }
    }
}

pub struct SccacheServer<C: CommandCreatorSync> {
    core: Core,
    listener: TcpListener,
    rx: mpsc::Receiver<ServerMessage>,
    timeout: Duration,
    service: SccacheService<C>,
    wait: WaitUntilZero,
}

impl<C: CommandCreatorSync> SccacheServer<C> {
    pub fn new(port: u16) -> io::Result<SccacheServer<C>> {
        let core = try!(Core::new());
        let handle = core.handle();
        let addr = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), port);
        let listener = try!(TcpListener::bind(&SocketAddr::V4(addr), &handle));

        // Prepare the service which we'll use to service all incoming TCP
        // connections.
        let (tx, rx) = mpsc::channel(1);
        let pool = CpuPool::new(20);
        let storage = storage_from_environment(&pool);
        let (wait, info) = WaitUntilZero::new();
        let service = SccacheService::new(storage, core.handle(), pool, tx, info);

        Ok(SccacheServer {
            core: core,
            listener: listener,
            rx: rx,
            service: service,
            timeout: Duration::from_millis(DEFAULT_IDLE_TIMEOUT),
            wait: wait,
        })
    }

    /// Configures how long this server will be idle before shutting down.
    #[allow(dead_code)]
    pub fn set_idle_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Set the `force_recache` setting.
    #[allow(dead_code)]
    pub fn set_force_recache(&mut self, force_recache: bool) {
        self.service.force_recache = force_recache;
    }

    /// Set the `force_recache` setting.
    #[allow(dead_code)]
    pub fn set_storage(&mut self, storage: Arc<Storage>) {
        self.service.storage = storage;
    }

    /// Returns a reference to the thread pool this server will run work on
    #[allow(dead_code)]
    pub fn pool(&self) -> &CpuPool {
        &self.service.pool
    }

    /// Returns a reference to the command creator this server will use
    #[allow(dead_code)]
    pub fn command_creator(&self) -> &C {
        &self.service.creator
    }

    /// Returns the port that this server is bound to
    #[allow(dead_code)]
    pub fn port(&self) -> u16 {
        self.listener.local_addr().unwrap().port()
    }

    /// Returns a handle to the event loop that this server is bound to.
    #[allow(dead_code)]
    pub fn handle(&self) -> Handle {
        self.core.handle()
    }

    /// Runs this server to completion.
    ///
    /// If the `shutdown` future resolves then the server will be shut down,
    /// otherwise the server may naturally shut down if it becomes idle for too
    /// long anyway.
    pub fn run<F>(self, shutdown: F) -> io::Result<()>
        where F: Future,
    {
        self._run(Box::new(shutdown.then(|_| Ok(()))))
    }

    fn _run<'a>(self, shutdown: Box<Future<Item = (), Error = ()> + 'a>)
                -> io::Result<()>
    {
        let SccacheServer { mut core, listener, rx, service, timeout, wait } = self;

        // Create our "server future" which will simply handle all incoming
        // connections in separate tasks.
        let handle = core.handle();
        let server = listener.incoming().for_each(move |(socket, _addr)| {
            SccacheProto.bind_server(&handle, socket, service.clone());
            Ok(())
        });

        // Right now there's a whole bunch of ways to shut down this server for
        // various purposes. These include:
        //
        // 1. The `shutdown` future above.
        // 2. An RPC indicating the server should shut down
        // 3. A period of inactivity (no requests serviced)
        //
        // These are all encapsulated wih the future that we're creating below.
        // The `ShutdownOrInactive` indicates the RPC or the period of
        // inactivity, and this is then select'd with the `shutdown` future
        // passed to this function.
        let handle = core.handle();
        let shutdown_idle = ShutdownOrInactive {
            rx: rx,
            timeout: try!(Timeout::new(timeout, &handle)),
            handle: handle.clone(),
            timeout_dur: timeout,
        };

        let server = server.select(shutdown_idle);
        let shutdown = shutdown.map_err(|()| {
            io::Error::new(io::ErrorKind::Other, "shutdown signal failed")
        });
        let server = server.map(|_| ()).map_err(|e| e.0).select(shutdown);
        try!(core.run(server)
                 .map_err(|p| p.0));

        // Once our server has shut down either due to inactivity or a manual
        // request we still need to give a bit of time for all active
        // connections to finish. This `wait` future will resolve once all
        // instances of `SccacheService` have been dropped.
        //
        // Note that we cap the amount of time this can take, however, as we
        // don't want to wait *too* long.
        let timeout = try!(Timeout::new(Duration::new(1, 0), &handle));
        try!(core.run(wait.select(timeout))
                 .map_err(|p| p.0));

        Ok(())
    }
}

/// Service implementation for sccache
#[derive(Clone)]
struct SccacheService<C: CommandCreatorSync> {
    /// Server statistics.
    stats: Rc<RefCell<ServerStats>>,

    /// Cache storage.
    storage: Arc<Storage>,

    /// A cache of known compiler info.
    compilers: Rc<RefCell<HashMap<String, Option<Compiler>>>>,

    /// True if all compiles should be forced, ignoring existing cache entries.
    ///
    /// This can be controlled with the `SCCACHE_RECACHE` environment variable.
    force_recache: bool,

    /// Thread pool to execute work in
    pool: CpuPool,

    /// Handle to the event loop that we're running on.
    handle: Handle,

    /// An object for creating commands.
    ///
    /// This is mostly useful for unit testing, where we
    /// can mock this out.
    creator: C,

    /// Message channel used to learn about requests received by this server.
    tx: mpsc::Sender<ServerMessage>,

    /// Information tracking how many services are active.
    info: ActiveInfo,
}

type SccacheRequest = Message<ClientRequest, Body<(), io::Error>>;
type SccacheResponse = Message<ServerResponse, Body<ServerResponse, io::Error>>;

pub enum ServerMessage {
    /// A message sent whenever a request is received.
    Request,
    /// Message sent whenever a shutdown request is received.
    Shutdown,
}

impl<C> Service for SccacheService<C>
    where C: CommandCreatorSync + 'static,
{
    type Request = SccacheRequest;
    type Response = SccacheResponse;
    type Error = io::Error;
    type Future = Box<Future<Item = Self::Response, Error = Self::Error>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let mut req = req.into_inner();
        trace!("handle_client");

        // Opportunistically let channel know that we've received a request. We
        // ignore failures here as well as backpressure as it's not imperative
        // that every message is received.
        drop(self.tx.clone().start_send(ServerMessage::Request));

        if req.has_compile() {
            debug!("handle_client: compile");
            self.stats.borrow_mut().compile_requests += 1;
            self.handle_compile(req.take_compile())
        } else {
            // Simple requests that can generate responses right away.
            let mut res = ServerResponse::new();
            if req.has_get_stats() {
                debug!("handle_client: get_stats");
                res.set_stats(self.get_stats());
            } else if req.has_zero_stats() {
                debug!("handle_client: zero_stats");
                res.set_stats(self.zero_stats());
            } else if req.has_shutdown() {
                debug!("handle_client: shutdown");
                let future = self.tx.clone().send(ServerMessage::Shutdown);
                let me = self.clone();
                return Box::new(future.then(move |_| {
                    let mut shutting_down = ShuttingDown::new();
                    shutting_down.set_stats(me.get_stats());
                    res.set_shutting_down(shutting_down);
                    Ok(Message::WithoutBody(res))
                }))
            } else {
                warn!("handle_client: unknown command");
                res.set_unknown(UnknownCommand::new());
            }

            future::ok(Message::WithoutBody(res)).boxed()
        }
    }
}

impl<C> SccacheService<C>
    where C: CommandCreatorSync,
{
    pub fn new(storage: Arc<Storage>,
               handle: Handle,
               pool: CpuPool,
               tx: mpsc::Sender<ServerMessage>,
               info: ActiveInfo) -> SccacheService<C> {
        SccacheService {
            stats: Rc::new(RefCell::new(ServerStats::default())),
            storage: storage,
            compilers: Rc::new(RefCell::new(HashMap::new())),
            force_recache: env::var("SCCACHE_RECACHE").is_ok(),
            pool: pool,
            handle: handle,
            creator: C::new(),
            tx: tx,
            info: info,
        }
    }

    /// Get stats about the cache.
    fn get_stats(&self) -> CacheStats {
        let mut stats = CacheStats::new();
        let mut stats_vec = self.stats.borrow().to_cache_statistics();

        let mut stat = CacheStatistic::new();
        stat.set_name(String::from("Cache location"));
        stat.set_str(self.storage.location());
        stats_vec.insert(0, stat);

        for &(s, v) in [("Cache size", self.storage.current_size()),
                       ("Max cache size", self.storage.max_size())].iter() {
            v.map(|val| {
                let mut stat = CacheStatistic::new();
                stat.set_name(String::from(s));
                stat.set_size(val as u64);
                stats_vec.insert(0, stat);
            });
        }

        stats.set_stats(RepeatedField::from_vec(stats_vec));
        stats
    }

    /// Zero and return stats about the cache.
    fn zero_stats(&self) -> CacheStats {
        *self.stats.borrow_mut() = ServerStats::default();
        self.get_stats()
    }


    /// Handle a compile request from a client.
    ///
    /// This will either start compilation and set a `CompileStarted`
    /// response in `res`, or set an `UnhandledCompile` response in `res`.
    fn handle_compile(&self, mut compile: Compile)
                      -> Box<Future<Item = SccacheResponse, Error = io::Error>>
    {
        let exe = compile.take_exe();
        let cmd = compile.take_command().into_vec();
        let cwd = compile.take_cwd();
        // See if this compiler is already in the cache.
        match self.compiler_info_cached(&exe) {
            Some(info) => {
                trace!("compiler_info cache hit");
                future::ok(self.check_compiler(info, cmd, cwd)).boxed()
            }
            None => {
                trace!("compiler_info cache miss");
                // Run a Task to check the compiler type.
                let exe2 = exe.clone();
                let me = self.clone();
                let creator = me.creator.clone();

                let info = self.pool.spawn_fn(move || {
                    Ok(get_compiler_info(creator, &exe2))
                });

                let me = self.clone();
                Box::new(info.map(move |info| {
                    me.cache_compiler_info(exe, &info);
                    me.check_compiler(info, cmd, cwd)
                }))
            }
        }
    }

    /// Look up compiler info from the cache for the compiler `path`.
    fn compiler_info_cached(&self, path: &str) -> Option<Option<Compiler>> {
        trace!("compiler_info_cached");
        match metadata(path) {
            Ok(attr) => {
                let mtime = FileTime::from_last_modification_time(&attr);
                match self.compilers.borrow().get(path) {
                    // It's a hit only if the mtime matches.
                    Some(&Some(ref c)) if c.mtime == mtime => Some(Some(c.clone())),
                    // We cache non-results.
                    Some(&None) => Some(None),
                    _ => None,
                }
            }
            Err(_) => None,
        }
    }

    /// Store `info` in the compiler info cache for `path`.
    fn cache_compiler_info(&self, path: String, info: &Option<Compiler>) {
        self.compilers.borrow_mut().insert(path, info.clone());
    }

    fn check_compiler(&self,
                      compiler: Option<Compiler>,
                      cmd: Vec<String>,
                      cwd: String)
                      -> SccacheResponse {
        let mut res = ServerResponse::new();
        match compiler {
            None => {
                debug!("check_compiler: Unsupported compiler");
                self.stats.borrow_mut().requests_unsupported_compiler += 1;
            }
            Some(c) => {
                debug!("check_compiler: Supported compiler");
                // Now check that we can handle this compiler with
                // the provided commandline.
                match c.parse_arguments(&cmd, cwd.as_ref()) {
                    CompilerArguments::Ok(args) => {
                        self.stats.borrow_mut().requests_executed += 1;
                        res.set_compile_started(CompileStarted::new());
                        let (tx, rx) = Body::pair();
                        self.start_compile_task(c, args, cmd, cwd, tx);
                        return Message::WithBody(res, rx)
                    }
                    CompilerArguments::CannotCache => {
                        self.stats.borrow_mut().requests_not_cacheable += 1;
                    }
                    CompilerArguments::NotCompilation => {
                        self.stats.borrow_mut().requests_not_compile += 1;
                    }
                }
            }
        }

        res.set_unhandled_compile(UnhandledCompile::new());
        Message::WithoutBody(res)
    }

    /// Start running `cmd` in a background task, in `cwd`.
    fn start_compile_task(&self,
                          compiler: Compiler,
                          parsed_arguments: ParsedArguments,
                          arguments: Vec<String>,
                          cwd: String,
                          tx: mpsc::Sender<io::Result<ServerResponse>>) {
        let cache_control = if self.force_recache {
            CacheControl::ForceRecache
        } else {
            CacheControl::Default
        };
        let result = compiler.get_cached_or_compile(&self.creator,
                                                    &self.storage,
                                                    &arguments,
                                                    &parsed_arguments,
                                                    &cwd,
                                                    cache_control,
                                                    &self.pool);
        let me = self.clone();
        let task = result.then(|result| {
            let mut res = ServerResponse::new();
            let mut finish = CompileFinished::new();
            let mut cache_write = None;
            match result {
                Ok((compiled, out)) => {
                    match compiled {
                        CompileResult::Error => {
                            me.stats.borrow_mut().cache_errors += 1;
                        }
                        CompileResult::CacheHit(duration) => {
                            me.stats.borrow_mut().cache_hits += 1;
                            me.stats.borrow_mut().cache_read_hit_duration += duration;
                        },
                        CompileResult::CacheMiss(miss_type, duration, future) => {
                            match miss_type {
                                MissType::Normal => {
                                    me.stats.borrow_mut().cache_misses += 1;
                                }
                                MissType::CacheReadError => {
                                    me.stats.borrow_mut().cache_read_errors += 1;
                                }
                                MissType::ForcedRecache => {
                                    me.stats.borrow_mut().cache_misses += 1;
                                    me.stats.borrow_mut().forced_recaches += 1;
                                }
                            }
                            me.stats.borrow_mut().cache_read_miss_duration += duration;
                            cache_write = Some(future);
                        }
                        CompileResult::NotCacheable => {
                            me.stats.borrow_mut().cache_misses += 1;
                            me.stats.borrow_mut().non_cacheable_compilations += 1;
                        }
                        CompileResult::CompileFailed => {
                            me.stats.borrow_mut().compile_fails += 1;
                        }
                    };
                    let Output { status, stdout, stderr } = out;
                    status.code()
                        .map_or_else(
                            || trace!("CompileFinished missing retcode"),
                            |s| { trace!("CompileFinished retcode: {}", s); finish.set_retcode(s) });
                    //TODO: sort out getting signal return on Unix
                    finish.set_stdout(stdout);
                    finish.set_stderr(stderr);
                }
                Err(_) => {
                    me.stats.borrow_mut().cache_errors += 1;
                    //TODO: figure out a better way to communicate this?
                    finish.set_retcode(-2);
                }
            };
            res.set_compile_finished(finish);
            let send = tx.send(Ok(res));

            let cache_write = cache_write.then(move |result| {
                match result {
                    Err(e) => {
                        debug!("Error executing cache write: {}", e);
                        me.stats.borrow_mut().cache_write_errors += 1;
                    }
                    //TODO: save cache stats!
                    Ok(Some(info)) => {
                        debug!("[{}]: Cache write finished in {}.{:03}s", info.object_file, info.duration.as_secs(), info.duration.subsec_nanos() / 1000_000);
                        me.stats.borrow_mut().cache_writes += 1;
                        me.stats.borrow_mut().cache_write_duration += info.duration;
                    }

                    Ok(None) => {}
                }
                Ok(())
            });

            send.join(cache_write).then(|_| Ok(()))
        });

        self.handle.spawn(task);
    }
}

/// Statistics about the cache.
struct ServerStats {
    /// The count of client compile requests.
    pub compile_requests: u64,
    /// The count of client requests that used an unsupported compiler.
    pub requests_unsupported_compiler: u64,
    /// The count of client requests that were not compilation.
    pub requests_not_compile: u64,
    /// The count of client requests that were not cacheable.
    pub requests_not_cacheable: u64,
    /// The count of client requests that were executed.
    pub requests_executed: u64,
    /// The count of errors handling compile requests.
    pub cache_errors: u64,
    /// The count of cache hits for handled compile requests.
    pub cache_hits: u64,
    /// The count of cache misses for handled compile requests.
    pub cache_misses: u64,
    /// The count of compilations which were successful but couldn't be cached.
    pub non_cacheable_compilations: u64,
    /// The count of compilations which forcibly ignored the cache.
    pub forced_recaches: u64,
    /// The count of errors reading from cache.
    pub cache_read_errors: u64,
    /// The count of errors writing to cache.
    pub cache_write_errors: u64,
    /// The number of successful cache writes.
    pub cache_writes: u64,
    /// The total time spent writing cache entries.
    pub cache_write_duration: Duration,
    /// The total time spent reading cache hits.
    pub cache_read_hit_duration: Duration,
    /// The total time spent reading cache misses.
    pub cache_read_miss_duration: Duration,
    /// The count of compilation failures.
    pub compile_fails: u64,
}

impl Default for ServerStats {
    fn default() -> ServerStats {
        ServerStats {
            compile_requests: u64::default(),
            requests_unsupported_compiler: u64::default(),
            requests_not_compile: u64::default(),
            requests_not_cacheable: u64::default(),
            requests_executed: u64::default(),
            cache_errors: u64::default(),
            cache_hits: u64::default(),
            cache_misses: u64::default(),
            non_cacheable_compilations: u64::default(),
            forced_recaches: u64::default(),
            cache_read_errors: u64::default(),
            cache_write_errors: u64::default(),
            cache_writes: u64::default(),
            cache_write_duration: Duration::new(0, 0),
            cache_read_hit_duration: Duration::new(0, 0),
            cache_read_miss_duration: Duration::new(0, 0),
            compile_fails: u64::default(),
        }
    }
}

impl ServerStats {
    fn to_cache_statistics(&self) -> Vec<CacheStatistic> {
        macro_rules! set_stat {
            ($vec:ident, $var:expr, $name:expr) => {{
                let mut stat = CacheStatistic::new();
                stat.set_name(String::from($name));
                stat.set_count($var);
                $vec.push(stat);
            }};
        }

        macro_rules! set_duration_stat {
            ($vec:ident, $dur:expr, $num:expr, $name:expr) => {{
                let mut stat = CacheStatistic::new();
                stat.set_name(String::from($name));
                if $num > 0 {
                    let duration = $dur / $num as u32;
                    stat.set_str(format!("{}.{:03} s", duration.as_secs(), duration.subsec_nanos() / 1000_000));
                } else {
                    stat.set_str("0.000 s".to_owned());
                }
                $vec.push(stat);
            }};
        }

        let mut stats_vec = vec!();
        set_stat!(stats_vec, self.compile_requests, "Compile requests");
        set_stat!(stats_vec, self.requests_executed, "Compile requests executed");
        set_stat!(stats_vec, self.cache_hits, "Cache hits");
        set_stat!(stats_vec, self.cache_misses, "Cache misses");
        set_stat!(stats_vec, self.forced_recaches, "Forced recaches");
        set_stat!(stats_vec, self.cache_read_errors, "Cache read errors");
        set_stat!(stats_vec, self.cache_write_errors, "Cache write errors");
        set_stat!(stats_vec, self.compile_fails, "Compilation failures");
        set_stat!(stats_vec, self.cache_errors, "Cache errors");
        set_stat!(stats_vec, self.non_cacheable_compilations, "Successful compilations which could not be cached");
        set_stat!(stats_vec, self.requests_not_cacheable, "Non-cacheable calls");
        set_stat!(stats_vec, self.requests_not_compile, "Non-compilation calls");
        set_stat!(stats_vec, self.requests_unsupported_compiler, "Unsupported compiler calls");
        set_duration_stat!(stats_vec, self.cache_write_duration, self.cache_writes, "Average cache write");
        set_duration_stat!(stats_vec, self.cache_read_miss_duration, self.cache_misses, "Average cache read miss");
        set_duration_stat!(stats_vec, self.cache_read_hit_duration, self.cache_hits, "Average cache read hit");
        stats_vec
    }
}

/// tokio-proto protocol implementation for sccache
struct SccacheProto;

impl<I> ServerProto<I> for SccacheProto
    where I: Io + 'static,
{
    type Request = ClientRequest;
    type RequestBody = ();
    type Response = ServerResponse;
    type ResponseBody = ServerResponse;
    type Error = io::Error;
    type Transport = SccacheTransport<I>;
    type BindTransport = future::FutureResult<Self::Transport, io::Error>;

    fn bind_transport(&self, io: I) -> Self::BindTransport {
        future::ok(SccacheTransport {
            inner: io.framed(ProtobufCodec::new()),
        })
    }
}

/// Implementation of `Stream + Sink` that tokio-proto is expecting. This takes
/// a `Framed` instance using `ProtobufCodec` and performs a simple map
/// operation on the sink/stream halves to translate the protobuf message types
/// to the `Frame` types that tokio-proto expects.
struct SccacheTransport<I> {
    inner: Framed<I, ProtobufCodec<ClientRequest, ServerResponse>>,
}

impl<I: Io> Stream for SccacheTransport<I> {
    type Item = Frame<ClientRequest, (), io::Error>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, io::Error> {
        let msg = try_ready!(self.inner.poll());
        Ok(msg.map(|m| {
            Frame::Message {
                message: m,
                body: false,
            }
        }).into())
    }
}

impl<I: Io> Sink for SccacheTransport<I> {
    type SinkItem = Frame<ServerResponse, ServerResponse, io::Error>;
    type SinkError = io::Error;

    fn start_send(&mut self, item: Self::SinkItem)
                  -> StartSend<Self::SinkItem, io::Error> {
        match item {
            Frame::Message { message, body } => {
                match try!(self.inner.start_send(message)) {
                    AsyncSink::Ready => Ok(AsyncSink::Ready),
                    AsyncSink::NotReady(message) => {
                        Ok(AsyncSink::NotReady(Frame::Message {
                            message: message,
                            body: body,
                        }))
                    }
                }
            }
            Frame::Body { chunk: Some(chunk) } => {
                match try!(self.inner.start_send(chunk)) {
                    AsyncSink::Ready => Ok(AsyncSink::Ready),
                    AsyncSink::NotReady(chunk) => {
                        Ok(AsyncSink::NotReady(Frame::Body {
                            chunk: Some(chunk),
                        }))
                    }
                }
            }
            Frame::Body { chunk: None } => Ok(AsyncSink::Ready),
            Frame::Error { error } => Err(error),
        }
    }

    fn poll_complete(&mut self) -> Poll<(), io::Error> {
        self.inner.poll_complete()
    }
}

impl<I: Io + 'static> Transport for SccacheTransport<I> {}

/// Simple tokio-core `Codec` which uses stock protobuf functions to
/// decode/encode protobuf messages.
struct ProtobufCodec<Request, Response> {
    _marker: marker::PhantomData<fn() -> (Request, Response)>,
}

impl<Request, Response> ProtobufCodec<Request, Response>
    where Request: protobuf::Message + protobuf::MessageStatic,
          Response: protobuf::Message,
{
    fn new() -> ProtobufCodec<Request, Response> {
        ProtobufCodec { _marker: marker::PhantomData }
    }
}

impl<Request, Response> Codec for ProtobufCodec<Request, Response>
    where Request: protobuf::Message + protobuf::MessageStatic,
          Response: protobuf::Message,
{
    type In = Request;
    type Out = Response;

    fn decode(&mut self, buf: &mut EasyBuf) -> io::Result<Option<Request>> {
        // TODO: this seems like a bug in upstream tokio-core that we're called
        //       with an empty buffer
        if buf.as_slice().len() == 0 {
            return Ok(None)
        }
        match parse_length_delimited_from_bytes::<Request>(buf.as_slice()) {
            Ok(req) => {
                // We need to tell `buf` how many bytes we just consumed, and we
                // might think to use `req.compute_size()`. Unfortunately that
                // return value doesn't include the leading bytes saying how
                // large the payload is, so to learn how many bytes we just
                // decoded we do the lame thing of re-encoding it and seeing how
                // many bytes that took.
                //
                // Should figure out a better way of doing this!
                let size = req.write_length_delimited_to_bytes().unwrap().len();
                buf.drain_to(size);
                Ok(Some(req))
            }
            // Unexpected EOF is OK, just means we haven't read enough
            // bytes. It would be nice if this were discriminated more
            // usefully.
            // Issue filed: https://github.com/stepancheg/rust-protobuf/issues/154
            Err(ProtobufError::WireError(s)) => {
                if s == "truncated message" {
                    Ok(None)
                } else {
                    Err(io::Error::new(ErrorKind::Other, s))
                }
            }
            Err(ProtobufError::IoError(ioe)) => Err(ioe),
            Err(ProtobufError::MessageNotInitialized { message }) => {
                Err(io::Error::new(ErrorKind::Other, message))
            }
        }
    }

    fn encode(&mut self, msg: Response, buf: &mut Vec<u8>) -> io::Result<()> {
        let bytes = try!(msg.write_length_delimited_to_bytes().map_err(|e| {
            io::Error::new(io::ErrorKind::Other, e)
        }));
        buf.extend_from_slice(&bytes);
        Ok(())
    }
}

struct ShutdownOrInactive {
    rx: mpsc::Receiver<ServerMessage>,
    handle: Handle,
    timeout: Timeout,
    timeout_dur: Duration,
}

impl Future for ShutdownOrInactive {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        loop {
            match self.rx.poll().unwrap() {
                Async::NotReady => break,
                // Shutdown received!
                Async::Ready(Some(ServerMessage::Shutdown)) => return Ok(().into()),
                Async::Ready(Some(ServerMessage::Request)) => {
                    self.timeout = try!(Timeout::new(self.timeout_dur, &self.handle));
                }
                // All services have shut down, in theory this isn't possible...
                Async::Ready(None) => return Ok(().into()),
            }
        }
        self.timeout.poll()
    }
}

/// Helper future which tracks the `ActiveInfo` below. This future will resolve
/// once all instances of `ActiveInfo` have been dropped.
struct WaitUntilZero {
    info: Weak<RefCell<Unblock>>,
}

#[derive(Clone)]
struct ActiveInfo {
    info: Rc<RefCell<Unblock>>,
}

struct Unblock {
    slot: Option<Task>,
}

impl WaitUntilZero {
    fn new() -> (WaitUntilZero, ActiveInfo) {
        let info = Rc::new(RefCell::new(Unblock { slot: None }));
        (WaitUntilZero { info: Rc::downgrade(&info) }, ActiveInfo { info: info })
    }
}

impl Drop for Unblock {
    fn drop(&mut self) {
        if let Some(task) = self.slot.take() {
            task.unpark();
        }
    }
}

impl Future for WaitUntilZero {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        match self.info.upgrade() {
            Some(info) => {
                info.borrow_mut().slot = Some(task::park());
                Ok(Async::NotReady)
            }
            None => Ok(().into()),
        }
    }
}
