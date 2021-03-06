use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use structopt::StructOpt;

type StdError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Debug, StructOpt)]
struct Opt {
    #[structopt(short = "i", long = "interface")]
    interface: String,

    #[structopt(short = "p", long = "port")]
    ports: Vec<u16>,
}

fn dump_ctrs(
    interval: std::time::Duration,
    first_done: tokio::sync::oneshot::Sender<()>,
    ctrs: Vec<(u16, Arc<AtomicUsize>)>,
) {
    loop {
        std::thread::sleep(interval);
        let counts: Vec<_> = ctrs
            .iter()
            .map(|(port, ctr)| (port, ctr.load(Ordering::Relaxed)))
            .collect();

        if counts[0].1 > 0 {
            break;
        }
    }

    // we got the first request.
    // now, start the timer on sharding so we can see the difference
    first_done.send(()).unwrap();

    loop {
        std::thread::sleep(interval);
        let counts: Vec<_> = ctrs
            .iter()
            .map(|(port, ctr)| (port, ctr.load(Ordering::Relaxed)))
            .collect();

        tracing::info!(counts = ?&counts, "request counts");
    }
}

async fn serve_udp(srv: rpcbench::Server, port: u16) -> Result<(), StdError> {
    // udp server
    let mut buf = [0u8; 1024];
    let addr: std::net::SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();
    let mut sk = tokio::net::UdpSocket::bind::<std::net::SocketAddr>(addr)
        .await
        .unwrap();
    loop {
        let (len, from_addr) = sk.recv_from(&mut buf).await?;
        let msg = &buf[..len];
        // deserialize
        let msg: rpcbench::SPingParams = bincode::deserialize(msg)?;
        let resp: rpcbench::SPong = srv.do_ping(msg.into()).await?.into();
        let msg = bincode::serialize(&resp)?;
        sk.send_to(&msg, from_addr).await?;
    }
}

#[tokio::main]
async fn main() -> Result<(), StdError> {
    let opt = Opt::from_args();

    tracing_subscriber::fmt::init();

    // listen on ports
    let ctrs: Vec<(u16, Arc<AtomicUsize>)> = opt
        .ports
        .clone()
        .into_iter()
        .map(|port| {
            let srv = rpcbench::Server::default();
            let ctr = srv.get_counter();
            tokio::spawn(serve_udp(srv, port));
            (port, ctr)
        })
        .collect();

    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || dump_ctrs(std::time::Duration::from_secs(1), tx, ctrs));

    let (start_sharding_tx, start_sharding_rx) = std::sync::mpsc::channel();
    tokio::spawn(async move {
        // after this, wait say 10 seconds and then enable sharding
        rx.await.unwrap();
        std::thread::sleep(std::time::Duration::from_secs(5));
        start_sharding_tx.send(()).unwrap();
    });

    let mut prog = xdp_shard::BpfHandles::load_on_interface_name(&opt.interface)?;
    let ifn = opt.interface;

    let stop: Arc<AtomicBool> = Arc::new(false.into());
    let s = stop.clone();
    ctrlc::set_handler(move || {
        tracing::warn!("stopping");
        s.store(true, Ordering::SeqCst);
    })
    .unwrap();

    // start with no sharding, then introduce sharding in a bit
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        std::time::Duration::from_secs(1);
        if let Ok(_) = start_sharding_rx.try_recv() {
            // PingParams is { i32, i64 } and we want to shard on the first value.
            // so offset = 0, length = 4.
            prog.shard_ports(opt.ports[0], &opt.ports[1..], 0, 4)?;
            tracing::info!(interface = ?&ifn, from = opt.ports[0], to = ?&opt.ports[1..], "sharding activated");
        }

        let (stats, prev) = prog.get_stats()?;

        let mut rxqs = stats.get_rxq_cpu_port_count();
        let prev_rxqs = prev.get_rxq_cpu_port_count();
        xdp_shard::diff_maps(&mut rxqs, &prev_rxqs);
        for (rxq, cpus) in rxqs.iter().enumerate() {
            for (cpu, portcounts) in cpus.iter().enumerate() {
                for (port, count) in portcounts.iter() {
                    if *count > 0 {
                        tracing::info!(interface = ?&ifn, rxq, cpu, port, count, "");
                    }
                }
            }
        }
    }

    Ok(())
}
