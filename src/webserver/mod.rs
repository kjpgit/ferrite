use std::sync::Arc;
use std::io::{TcpListener, TcpStream};
use std::io::net::tcp::TcpAcceptor;
use std::io::{Acceptor, Listener};

pub use self::request::WebRequest;
pub use self::response::WebResponse;

use threadpool::ThreadPool;
use byteutils;

mod request;
mod response;


/*

Main thread:
- creates listening socket
- starts worker threads
- waits on worker_monitor condition var, to see when threads need respawn

Worker threads:
- clone the listening socket, so they can each call accept().  no context
  switches needed.
- passed the global configuraiton and dispatch map (read only)

Note that for linux kernel >= 3.9, an optimization (for VERY high req/sec
loads) is to use SO_REUSEPORT so each worker thread has a different socket.
That would be a trivial change, and not affect this architecture.

*/


pub type PageFunction = fn(&WebRequest) -> WebResponse;

struct DispatchRule {
    prefix: String,
    page_fn: PageFunction
}

struct WorkerSharedContext {
    rules: Vec<DispatchRule>,
    acceptor: TcpAcceptor,
}

struct WorkerPrivateContext {
    shared_ctx: Arc<WorkerSharedContext>,
}



pub struct WebServer {
    rules: Option<Vec<DispatchRule>>,
    thread_pool: ThreadPool,
    worker_shared_context: Option<Arc<WorkerSharedContext>>,
}

impl WebServer {
    pub fn new() -> WebServer {
        let ret = WebServer{
                rules: Some(Vec::new()),
                thread_pool: ThreadPool::new(),
                worker_shared_context: None,
            };
        return ret;
    }

    pub fn add_path(&mut self, path: &str, page_fn: PageFunction) {
        let fn_map = self.rules.as_mut().unwrap();
        let rule = DispatchRule { 
            prefix: path.to_string(), 
            page_fn: page_fn 
        };
        fn_map.push(rule);
    }

    /// Starts `num_threads` worker threads.  If any fail, they will be
    /// respawned.  This function does not return.
    pub fn run(&mut self, address: &str, port: i32, num_threads: i32) {
        let addr = format!("{}:{}", address, port);
        println!("listening on {}", addr);
        let listener = TcpListener::bind(addr.as_slice());
        let acceptor = listener.listen().unwrap();
        
        // .clone doesn't work, compiler bug
        let page_fn_copy = self.rules.take().unwrap();

        let ctx = WorkerSharedContext {
            rules: page_fn_copy,
            acceptor: acceptor,
        };
        self.worker_shared_context = Some(Arc::new(ctx));

        println!("starting {} worker threads", num_threads);
        for _ in range(0, num_threads) {
            self.start_new_worker();
        }

        println!("starting monitor loop");
        loop {
            self.thread_pool.wait_for_thread_exit();
            println!("uh oh, a worker thread died");
            println!("starting another worker");
            self.start_new_worker();
        }
    }

    fn start_new_worker(&mut self) {
        let priv_ctx = WorkerPrivateContext {
            shared_ctx: self.worker_shared_context.as_mut().unwrap().clone(),
        };
        self.thread_pool.execute(move || {
            worker_thread_main(priv_ctx);
        });
    }
}


fn worker_thread_main(ctx: WorkerPrivateContext) {
    let mut acceptor = ctx.shared_ctx.acceptor.clone();
    loop {
        let res = acceptor.accept();
        match res {
            Ok(sock) => process_http_connection(&ctx, sock),
            Err(err) => println!("socket error :-( {}", err)
        }
    }
}


// HTTP specific parsing/errors

fn process_http_connection(ctx: &WorkerPrivateContext, stream: TcpStream) {
    let mut sentinel = HTTPContext { 
        stream: stream, 
        started_response: false 
    };
    let req = read_request(&mut sentinel.stream);
    println!("parsed request ok: path={}", req.path);
    for rule in ctx.shared_ctx.rules.iter() {
        // todo: prefix
        if rule.prefix == req.path {
            let response = (rule.page_fn)(&req);
            sentinel.send_response(&response);
            return;
        }
    }

    println!("no rule matched {}", req.path);
    let mut response = WebResponse::new();
    response.code = 404;
    response.status = "Not Found, Bro".to_string();
    response.set_data(b"Error 404: Resource not found".to_vec());
    sentinel.send_response(&response);
}

struct HTTPContext {
    stream: TcpStream,
    started_response: bool,
}

impl HTTPContext {
    fn send_response(&mut self, response: &WebResponse) {
        // todo: don't panic if logging fails?
        println!("sending response: code={}, body_length={}",
            response.code, response.data.len());

        let mut resp = String::new();
        resp.push_str(format!("HTTP/1.1 {} {}\r\n", 
            response.code, 
            response.status).as_slice());
        resp.push_str("Connection: close\r\n");

        for (k, v) in response.headers.iter() {
            resp.push_str(k.as_slice());
            resp.push_str(": ");
            resp.push_str(v.as_slice());
            resp.push_str("\r\n");
        }

        resp.push_str("\r\n");

        // TODO: error check
        // We *don't* want to panic if we're already in a panic, and
        // sending the internal error message.
        self.started_response = true;
        let _ioret = self.stream.write_str(resp.as_slice());
        let _ioret = self.stream.write(response.data.as_slice());
    }
}

impl Drop for HTTPContext {
    /// If we paniced and/or are about to die, make sure client gets a 500
    fn drop(&mut self) {
        if !self.started_response {
            let mut resp = WebResponse::new();
            resp.code = 500;
            resp.status = "Uh oh :-(".to_string();
            resp.set_data(b"Error 500: Internal Error".to_vec());
            self.send_response(&resp);
        }
    }
}

fn read_request(stream: &mut TcpStream) -> WebRequest {
    // Read this amount at a time, if we want to set a max request size.
    let chunk_size = 4096;
    let mut req_buffer = Vec::<u8>::with_capacity(chunk_size);
    loop {
        let ioret = stream.push(chunk_size, &mut req_buffer);
        // todo: err handle
        let size = ioret.unwrap();
        //println!("read size {}", size);
        if size > 0 {
            //println!("req_buffer {}", req_buffer.len());
            let split_pos = byteutils::memmem(req_buffer.as_slice(), b"\r\n\r\n");
            if split_pos.is_some() {
                let split_pos = split_pos.unwrap();
                println!("read raw request: {} bytes", split_pos);
                return request::parse_request(req_buffer.as_slice());
            }
        }
    }
}