use lsmrs::hash::hash64;
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Instant;

const THETA: f64 = 0.99;

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

// Gray et al., as used by YCSB: a few keys take most of the traffic. The result
// is scrambled through the store's own hash so the hot set is scattered across
// the keyspace instead of sitting in one contiguous run.
struct Zipfian {
    n: u64,
    zetan: f64,
    eta: f64,
    alpha: f64,
}

impl Zipfian {
    fn new(n: u64) -> Self {
        let zetan = (1..=n).map(|i| 1.0 / (i as f64).powf(THETA)).sum::<f64>();
        let zeta2 = (1..=2).map(|i| 1.0 / (i as f64).powf(THETA)).sum::<f64>();
        let eta = (1.0 - (2.0 / n as f64).powf(1.0 - THETA)) / (1.0 - zeta2 / zetan);
        Self {
            n,
            zetan,
            eta,
            alpha: 1.0 / (1.0 - THETA),
        }
    }

    fn next(&self, rng: &mut Rng) -> u64 {
        let u = rng.next_f64();
        let uz = u * self.zetan;
        let raw = if uz < 1.0 {
            0
        } else if uz < 1.0 + 0.5f64.powf(THETA) {
            1
        } else {
            (self.n as f64 * (self.eta * u - self.eta + 1.0).powf(self.alpha)) as u64
        };
        hash64(&raw.to_le_bytes()) % self.n
    }
}

enum Distribution {
    Uniform(u64),
    Zipfian(Zipfian),
}

impl Distribution {
    fn next(&self, rng: &mut Rng) -> u64 {
        match self {
            Distribution::Uniform(n) => rng.next_u64() % n,
            Distribution::Zipfian(z) => z.next(rng),
        }
    }
}

struct Client {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
    scratch: Vec<u8>,
}

impl Client {
    fn connect(addr: &str) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        Ok(Self {
            writer: stream.try_clone()?,
            reader: BufReader::new(stream),
            scratch: Vec::with_capacity(256),
        })
    }

    fn call(&mut self, parts: &[&[u8]]) -> std::io::Result<()> {
        self.scratch.clear();
        write!(self.scratch, "*{}\r\n", parts.len())?;
        for part in parts {
            write!(self.scratch, "${}\r\n", part.len())?;
            self.scratch.extend_from_slice(part);
            self.scratch.extend_from_slice(b"\r\n");
        }
        self.writer.write_all(&self.scratch)?;
        self.consume_reply()
    }

    fn consume_reply(&mut self) -> std::io::Result<()> {
        let line = self.read_line()?;
        match line.as_bytes().first() {
            Some(b'$') => {
                let len: i64 = line[1..].trim().parse().unwrap_or(-1);
                if len >= 0 {
                    let mut body = vec![0u8; len as usize + 2];
                    self.reader.read_exact(&mut body)?;
                }
                Ok(())
            }
            Some(b'*') => {
                let count: i64 = line[1..].trim().parse().unwrap_or(0);
                for _ in 0..count.max(0) {
                    self.consume_reply()?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn read_line(&mut self) -> std::io::Result<String> {
        let mut line = String::new();
        let mut byte = [0u8; 1];
        loop {
            self.reader.read_exact(&mut byte)?;
            if byte[0] == b'\n' {
                return Ok(line);
            }
            if byte[0] != b'\r' {
                line.push(byte[0] as char);
            }
        }
    }
}

fn key(i: u64) -> Vec<u8> {
    format!("user{i:012}").into_bytes()
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index]
}

fn report(label: &str, mut reads: Vec<u64>, mut writes: Vec<u64>, elapsed: f64) {
    reads.sort_unstable();
    writes.sort_unstable();
    let total = reads.len() + writes.len();

    println!("\n{label}");
    println!("  throughput   {:.0} ops/sec", total as f64 / elapsed);
    println!(
        "  {:<8} {:>8} {:>9} {:>9} {:>9} {:>9}",
        "op", "count", "p50 us", "p95 us", "p99 us", "p99.9 us"
    );
    for (name, samples) in [("read", &reads), ("update", &writes)] {
        if samples.is_empty() {
            continue;
        }
        println!(
            "  {:<8} {:>8} {:>9.1} {:>9.1} {:>9.1} {:>9.1}",
            name,
            samples.len(),
            percentile(samples, 0.50) as f64 / 1000.0,
            percentile(samples, 0.95) as f64 / 1000.0,
            percentile(samples, 0.99) as f64 / 1000.0,
            percentile(samples, 0.999) as f64 / 1000.0,
        );
    }
}

fn arg(args: &[String], name: &str, fallback: &str) -> String {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| fallback.to_string())
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let addr = arg(&args, "--addr", "127.0.0.1:6379");
    let workload = arg(&args, "--workload", "a");
    let records: u64 = arg(&args, "--records", "50000").parse().unwrap();
    let ops: u64 = arg(&args, "--ops", "50000").parse().unwrap();
    let threads: u64 = arg(&args, "--threads", "8").parse().unwrap();
    let dist = arg(&args, "--distribution", "zipfian");

    let read_share = match workload.as_str() {
        "a" => 0.50,
        "b" => 0.95,
        "c" => 1.00,
        other => panic!("unknown workload {other}, expected a, b or c"),
    };

    let value = vec![b'x'; 100];

    let mut loader = Client::connect(&addr)?;
    let load_start = Instant::now();
    for i in 0..records {
        loader.call(&[b"SET", &key(i), &value])?;
    }
    println!(
        "loaded {records} records in {:.2}s ({:.0} ops/sec)",
        load_start.elapsed().as_secs_f64(),
        records as f64 / load_start.elapsed().as_secs_f64()
    );

    let start = Instant::now();
    let workers: Vec<_> = (0..threads)
        .map(|t| {
            let addr = addr.clone();
            let value = value.clone();
            let dist = dist.clone();
            std::thread::spawn(move || {
                let mut client = Client::connect(&addr).unwrap();
                let mut rng = Rng(0x2545_F491_4F6C_DD1D ^ (t + 1).wrapping_mul(0x9E37_79B9));
                let keys = match dist.as_str() {
                    "uniform" => Distribution::Uniform(records),
                    _ => Distribution::Zipfian(Zipfian::new(records)),
                };

                let per_thread = ops / threads;
                let mut reads = Vec::with_capacity(per_thread as usize);
                let mut writes = Vec::with_capacity(per_thread as usize);

                for _ in 0..per_thread {
                    let k = key(keys.next(&mut rng));
                    let is_read = rng.next_f64() < read_share;
                    let op_start = Instant::now();
                    if is_read {
                        client.call(&[b"GET", &k]).unwrap();
                    } else {
                        client.call(&[b"SET", &k, &value]).unwrap();
                    }
                    let nanos = op_start.elapsed().as_nanos() as u64;
                    if is_read {
                        reads.push(nanos);
                    } else {
                        writes.push(nanos);
                    }
                }
                (reads, writes)
            })
        })
        .collect();

    let mut reads = Vec::new();
    let mut writes = Vec::new();
    for worker in workers {
        let (r, w) = worker.join().unwrap();
        reads.extend(r);
        writes.extend(w);
    }

    report(
        &format!("workload {workload} / {dist} / {threads} threads -> {addr}"),
        reads,
        writes,
        start.elapsed().as_secs_f64(),
    );
    Ok(())
}
