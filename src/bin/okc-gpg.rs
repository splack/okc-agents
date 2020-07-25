extern crate base64;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate slog;
extern crate slog_async;
extern crate slog_envlogger;
extern crate slog_term;
extern crate tokio;
extern crate okc_agents;

use std::error::Error;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use slog::{Logger, Drain};
use slog_async::{Async, AsyncGuard};
use slog_term::{FullFormat, TermDecorator};
use tokio::prelude::*;
use tokio::fs::File;
use tokio::io;
use tokio::net::{TcpListener, TcpStream};
use tokio::stream::StreamExt;
use okc_agents::utils::*;

lazy_static! {
	static ref LOG_GUARD: Mutex<Option<AsyncGuard>> = Mutex::new(None);
}

fn exit_error(e: Box<dyn Error>, logger: Logger) -> ! {
	error!(logger, "{:?}", e);
	if let Some(guard) = LOG_GUARD.lock().unwrap().take() {
		std::mem::drop(guard);
	}
	std::process::exit(1)
}

async fn read_str<T: AsyncRead + Unpin>(rx: &mut T) -> std::result::Result<String, Box<dyn Error>> {
	let mut len_buf = [0u8; 2];
	rx.read_exact(&mut len_buf).await?;
	let len = ((len_buf[0] as usize) << 8) + len_buf[1] as usize;
	let mut str_buf = vec!(0u8; len);
	rx.read_exact(&mut str_buf).await?;
	Ok(String::from_utf8(str_buf)?)
}

async fn handle_control_connection(mut stream: TcpStream, logger: Logger) -> Result {
	info!(logger, "control connection established");
	loop {
		let msg = read_str(&mut stream).await?;
		debug!(logger, "new warning message received"; "length" => msg.len());
		match msg.is_empty() {
			true => break,
			false => warn!(logger, "{}", msg)
		}
	}
	debug!(logger, "all warnings processed, waiting for status code");
	let mut stat_buf = [0u8; 1];
	stream.read_exact(&mut stat_buf).await?;
	info!(logger, "control connection finished"; "status_code" => stat_buf[0]);
	match stat_buf[0] {
		0 => Ok(()),
		_ => Err(Box::new(StringError(String::from("an error has occurred in the app"))) as Box<dyn Error>)
	}
}

async fn handle_input_connection(mut stream: TcpStream, logger: Logger) -> Result {
	let path = read_str(&mut stream).await?;
	info!(logger, "input connection established"; "path" => &path);
	if &path == "-" {
		let mut stdin = io::stdin();
		debug!(logger, "reading from stdin");
		io::copy(&mut stdin, &mut stream).await?;
	} else {
		let mut file = File::open(&path).await?;
		debug!(logger, "reading from file");
		io::copy(&mut file, &mut stream).await?;
	}
	info!(logger, "input connection finished");
	Ok(())
}

async fn handle_output_connection(mut stream: TcpStream, logger: Logger) -> Result {
	let path = read_str(&mut stream).await?;
	info!(logger, "output connection established"; "path" => &path);
	if &path == "-" {
		let mut stdout = io::stdout();
		debug!(logger, "writing to stdout");
		io::copy(&mut stream, &mut stdout).await?;
	} else {
		let mut file = File::create(&path).await?;
		debug!(logger, "writing to file");
		io::copy(&mut stream, &mut file).await?;
	}
	info!(logger, "output connection finished");
	Ok(())
}

async fn handle_connection(accept_result: std::result::Result<TcpStream, tokio::io::Error>, logger: Logger) -> Result {
	let mut stream = accept_result?;
	let logger = logger.new(o!("remote_port" => stream.peer_addr()?.port()));
	debug!(logger, "connection accepted");
	let mut op = [0u8];
	stream.read_exact(&mut op).await?;
	debug!(logger, "connection type is {}", op[0]);
	match op[0] {
		0 => match handle_control_connection(stream, logger).await {
			Ok(_) => std::process::exit(0),
			Err(e) => Err(e)
		},
		1 => handle_input_connection(stream, logger).await,
		2 => handle_output_connection(stream, logger).await,
		_ => Err(Box::new(StringError(String::from("protocol error: invalid connection type"))) as Box<dyn Error>)
	}
}

async fn run(logger: Logger) -> Result {
	let addr = "127.0.0.1:0".parse::<SocketAddr>()?;
	let mut listener = TcpListener::bind(&addr).await?;
	let addr = listener.local_addr()?;
	info!(logger, "listening on port {}", addr.port());
	let mut cmd = Command::new("am");
	cmd.arg("broadcast")
		.arg("-n").arg("org.ddosolitary.okcagent/.GpgProxyReceiver")
		.arg("--ei").arg("org.ddosolitary.okcagent.extra.PROXY_PORT").arg(addr.port().to_string())
		.stdout(Stdio::null()).stderr(Stdio::null());
	if std::env::args().len() > 1 {
		cmd.arg("--esa").arg("org.ddosolitary.okcagent.extra.GPG_ARGS")
			.arg(std::env::args().skip(1).map(|s| base64::encode(&s)).collect::<Vec<_>>().join(","));
	} else {
		debug!(logger, "no arguments specified, GPG_ARGS won't be sent")
	}
	cmd.status()?;
	info!(logger, "broadcast sent, waiting for app to connect");
	let mut incoming = listener.incoming();
	while let Some(accept_result) = incoming.next().await {
		debug!(logger, "new incoming connection");
		if let Err(e) = handle_connection(accept_result, logger.clone()).await {
			exit_error(e, logger)
		}
	};
	Ok(())
}

#[tokio::main]
async fn main() {
	let drain = FullFormat::new(TermDecorator::new().stderr().build()).build().ignore_res();
	let drain = slog_envlogger::new(drain).ignore_res();
	let (drain, guard) = Async::new(drain).build_with_guard();
	*LOG_GUARD.lock().unwrap() = Some(guard);
	let logger = Logger::root(drain.ignore_res(), o!());
	if let Err(e) = run(logger.clone()).await {
		exit_error(e, logger);
	}
}
