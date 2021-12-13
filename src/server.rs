use std::process::exit;

use clap::{App, AppSettings, Arg, ArgSettings, SubCommand};
use fuso::parse_addr;
use fuso_core::{
    cipher::{Security, Xor},
    core::{Config, Fuso},
    dispatch::State,
    packet::{Action, Addr},
    Forward, FusoPacket, Spawn,
};

use fuso_socks::{AsyncUdpForward, Socks, Socks5Ex};
use futures::{AsyncReadExt, AsyncWriteExt, StreamExt, TryFutureExt};
use smol::Executor;

fn main() {
    let app = App::new("fuso")
        .version("v1.0.3")
        .author("https://github.com/editso/fuso")
        .arg(
            Arg::new("server-bind-host")
                .default_value("0.0.0.0")
                .long("host")
                .short('h')
                .display_order(1)
                .about("监听地址"),
        )
        .arg(
            Arg::new("server-bind-port")
                .default_value("9003")
                .long("port")
                .short('p')
                .display_order(2)
                .about("监听端口"),
        )
        .arg(
            Arg::new("xor-secret")
                .long("xor")
                .short('x')
                .default_value("27")
                .display_order(3)
                .validator(|num| {
                    num.parse::<u8>()
                        .map_or(Err(String::from("Invalid number 0-255")), |_| Ok(()))
                })
                .about("传输时使用异或加密的Key"),
        )
        .arg(
            Arg::new("log")
                .short('l')
                .display_order(4)
                .possible_values(["debug", "info", "trace", "error", "warn"])
                .default_value("info")
                .about("日志级别"),
        );

    let matches = app.get_matches();

    let server_bind_addr = parse_addr(
        matches.value_of("server-bind-host").unwrap(),
        matches.value_of("server-bind-port").unwrap(),
    );

    if server_bind_addr.is_err() {
        println!("Parameter error: {}", server_bind_addr.unwrap_err());
        exit(1);
    }

    let server_bind_addr = server_bind_addr.unwrap();

    let xor_num: u8 = matches.value_of("xor-secret").unwrap().parse().unwrap();

    env_logger::builder()
        .filter_level(match matches.value_of("log").unwrap() {
            "debug" => log::LevelFilter::Debug,
            "info" => log::LevelFilter::Info,
            "warn" => log::LevelFilter::Warn,
            "error" => log::LevelFilter::Error,
            "trace" => log::LevelFilter::Trace,
            _ => log::LevelFilter::Info,
        })
        .init();

    smol::block_on(async move {
        Fuso::builder()
            .with_config(Config {
                debug: false,
                bind_addr: server_bind_addr,
            })
            .chain_handler(|chain| {
                chain
                    .next(|mut tcp, _| async move {
                        let _ = tcp.begin().await;
                        let mut buf = Vec::new();
                        buf.resize(1024, 0);

                        let n = tcp.read(&mut buf).await?;
                        buf.truncate(n);
                       
                        if buf.starts_with(b"GET / HTTP/1.1") && buf.ends_with(b"\r\n\r\n") {
                            log::debug!("{}", String::from_utf8_lossy(&buf));
                            log::info!("Attempt to do a websocket handshake");
                            tcp.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n").await?;
                        }else{
                            let _ = tcp.back().await;
                        }

                        Ok(State::Next)
                    })
                    .next(|mut tcp, cx| async move {
                        let action: Action = tcp.recv().await?.try_into()?;
                        let _ = tcp.begin().await;
                        match action {
                            Action::TcpBind(name, addr) => {
                                match cx.spawn(tcp.clone(), addr, name).await {
                                    Ok(conv) => {
                                        log::debug!(
                                            "[fuso] accept conv={}, addr={}",
                                            conv,
                                            tcp.peer_addr().unwrap(),
                                        );
                                        Ok(State::Accept(()))
                                    }
                                    Err(e) => {
                                        log::warn!(
                                            "[fuso] Failed to open the mapping {}",
                                            e.to_string()
                                        );
                                        let _ = tcp.send(Action::Err(e.to_string()).into()).await?;
                                        Ok(State::Accept(()))
                                    }
                                }
                            }
                            Action::UdpBind(conv) => {
                                cx.route(conv, action, tcp).await?;
                                Ok(State::Accept(()))
                            }
                            Action::Connect(conv, _) => {
                                cx.route(conv, action, tcp).await?;
                                Ok(State::Accept(()))
                            }
                            _ => {
                                let _ = tcp.back().await;
                                Ok(State::Next)
                            }
                        }
                    })
            })
            .chain_strategy(|chain| {
                chain
                    .next(|tcp, core| async move {
                        let _ = tcp.begin().await;
                        match tcp.clone().authenticate(None).await {
                            Ok(Socks::Udp(forward)) => {
                                log::info!("[udp_forward] {}", tcp.peer_addr().unwrap());

                                core.udp_forward(|listen, mut udp| {
                                    forward.resolve(listen.local_addr(), || async move {
                                        let mut stream = listen.accept().await?;
                                        let packet = stream.recv_udp().await?;
                                        let addr = match packet.addr() {
                                            fuso_socks::Addr::Socket(addr) => {
                                                fuso_core::packet::Addr::Socket(addr)
                                            }
                                            fuso_socks::Addr::Domain(domain, port) => {
                                                fuso_core::packet::Addr::Domain(domain, port)
                                            }
                                        };

                                        let data = udp.call(addr, packet.data()).await?;

                                        stream.send_udp(([0, 0, 0, 0], 0).into(), &data).await?;

                                        Ok(())
                                    })
                                })
                                .await?;

                                Ok(State::Release)
                            }
                            Ok(Socks::Tcp(tcp, addr)) => Ok(State::Accept(Action::Forward(0, {
                                log::debug!("[socks] {}", addr);
                                let _ = tcp.release().await;
                                match addr {
                                    fuso_socks::Addr::Socket(addr) => {
                                        fuso_core::packet::Addr::Socket(addr)
                                    }
                                    fuso_socks::Addr::Domain(domain, port) => {
                                        fuso_core::packet::Addr::Domain(domain, port)
                                    }
                                }
                            }))),
                            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                                let _ = tcp.back().await;
                                Ok(State::Next)
                            }
                            Err(e) => Err(e.into()),
                        }
                    })
                    .next(|_, _| async move {
                        Ok(State::Accept(Action::Forward(
                            0,
                            Addr::Socket(([0, 0, 0, 0], 0).into()),
                        )))
                    })
            })
            .build()
            .map_ok(|fuso| {
                let ex = Executor::new();
                smol::block_on(ex.run(fuso.for_each(move |stream| async move {
                    let xor = Xor::new(xor_num);
                    let (from, to) = stream.split();

                    let to = to.cipher(xor.clone()).await;

                    from.forward(to).detach();
                })));
            })
            .map_err(|e| async move {
                log::error!("{}", e);
            })
            .await
            .ok()
    });
}
