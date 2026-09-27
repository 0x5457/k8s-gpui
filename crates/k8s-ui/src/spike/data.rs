//! Synthetic Deployment-style rows: 10,000 rows with 20 columns, generated in memory.
//!
//! A seeded linear congruential generator keeps the data reproducible without a dependency.

use gpui::SharedString;

pub const CELL_COUNT: usize = 20;

pub struct Column {
    pub title: &'static str,
    pub width: f32,
}

/// Fixed column widths total about 3090 px, which is wider than the window.
pub const COLUMNS: [Column; CELL_COUNT] = [
    Column {
        title: "Name",
        width: 320.0,
    },
    Column {
        title: "Namespace",
        width: 150.0,
    },
    Column {
        title: "Ready",
        width: 90.0,
    },
    Column {
        title: "Status",
        width: 130.0,
    },
    Column {
        title: "Up-to-Date",
        width: 100.0,
    },
    Column {
        title: "Available",
        width: 100.0,
    },
    Column {
        title: "Restarts",
        width: 90.0,
    },
    Column {
        title: "Age",
        width: 90.0,
    },
    Column {
        title: "Containers",
        width: 100.0,
    },
    Column {
        title: "Image",
        width: 340.0,
    },
    Column {
        title: "Pull Policy",
        width: 130.0,
    },
    Column {
        title: "Strategy",
        width: 120.0,
    },
    Column {
        title: "Selector",
        width: 260.0,
    },
    Column {
        title: "Labels",
        width: 280.0,
    },
    Column {
        title: "Node",
        width: 220.0,
    },
    Column {
        title: "QoS",
        width: 120.0,
    },
    Column {
        title: "CPU Request",
        width: 100.0,
    },
    Column {
        title: "Memory Request",
        width: 100.0,
    },
    Column {
        title: "Ports",
        width: 160.0,
    },
    Column {
        title: "Revision",
        width: 90.0,
    },
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ready,
    Progressing,
    Degraded,
}

pub struct Row {
    pub name: SharedString,
    pub namespace: SharedString,
    pub ready: SharedString,
    pub status: SharedString,
    pub up_to_date: SharedString,
    pub available: SharedString,
    pub restarts: SharedString,
    pub age: SharedString,
    pub containers: SharedString,
    pub image: SharedString,
    pub pull_policy: SharedString,
    pub strategy: SharedString,
    pub selector: SharedString,
    pub labels: SharedString,
    pub node: SharedString,
    pub qos: SharedString,
    pub cpu_request: SharedString,
    pub mem_request: SharedString,
    pub ports: SharedString,
    pub revision: SharedString,
    pub health: Health,
    /// Frame number of the last change. Zero means the row was never changed.
    pub modified: u64,
}

/// Use a simple generator for reproducible data without a dependency.
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next_u64() % bound
        }
    }

    fn hex(&mut self, digits: usize) -> String {
        let mut out = String::with_capacity(digits);
        for _ in 0..digits {
            let digit = (self.next_u64() % 16) as u8;
            out.push(char::from_digit(u32::from(digit), 16).unwrap_or('0'));
        }
        out
    }
}

const SERVICES: [&str; 10] = [
    "coredns",
    "metrics-server",
    "ingress-nginx",
    "cert-manager",
    "prometheus",
    "grafana",
    "loki",
    "argocd",
    "cluster-autoscaler",
    "external-dns",
];

fn service(rng: &mut Lcg) -> &'static str {
    SERVICES
        .get(rng.below(SERVICES.len() as u64) as usize)
        .copied()
        .unwrap_or("app")
}

fn namespace(rng: &mut Lcg) -> &'static str {
    match rng.below(6) {
        0 => "kube-system",
        1 => "default",
        2 => "monitoring",
        3 => "ingress-nginx",
        4 => "cert-manager",
        _ => "logging",
    }
}

fn format_age(seconds: u64) -> String {
    if seconds >= 86_400 {
        format!("{}d", seconds / 86_400)
    } else if seconds >= 3_600 {
        format!("{}h", seconds / 3_600)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

fn rollout(replicas: u64, ready: u64) -> Health {
    if ready == replicas {
        Health::Ready
    } else if ready == 0 {
        Health::Degraded
    } else {
        Health::Progressing
    }
}

fn status_text(health: Health) -> SharedString {
    match health {
        Health::Ready => SharedString::from("Running"),
        Health::Progressing => SharedString::from("Progressing"),
        Health::Degraded => SharedString::from("CrashLoopBackOff"),
    }
}

fn random_row(rng: &mut Lcg) -> Row {
    let service = service(rng);
    let image = format!(
        "registry.k8s.io/{service}:v1.{}.{}",
        rng.below(24),
        rng.below(9)
    );
    let replicas = 1 + rng.below(6);
    let ready = rng.below(replicas + 1);
    let health = rollout(replicas, ready);
    Row {
        name: SharedString::from(format!("{service}-{}", rng.hex(10))),
        namespace: SharedString::from(namespace(rng)),
        ready: SharedString::from(format!("{ready}/{replicas}")),
        status: status_text(health),
        up_to_date: SharedString::from(format!("{replicas}")),
        available: SharedString::from(format!("{ready}")),
        restarts: SharedString::from(format!("{}", rng.below(40))),
        age: SharedString::from(format_age(rng.below(400 * 86_400))),
        containers: SharedString::from(service),
        image: SharedString::from(image),
        pull_policy: SharedString::from("IfNotPresent"),
        strategy: SharedString::from("RollingUpdate"),
        selector: SharedString::from(format!(
            "app.kubernetes.io/name={service},app.kubernetes.io/instance={}",
            rng.hex(6)
        )),
        labels: SharedString::from(format!(
            "app={service},tier={},team={}",
            match rng.below(3) {
                0 => "backend",
                1 => "frontend",
                _ => "infra",
            },
            match rng.below(4) {
                0 => "platform",
                1 => "sre",
                2 => "payments",
                _ => "search",
            }
        )),
        node: SharedString::from(format!("node-{:02}.cluster.local", rng.below(24))),
        qos: SharedString::from(match rng.below(3) {
            0 => "Guaranteed",
            1 => "Burstable",
            _ => "BestEffort",
        }),
        cpu_request: SharedString::from(format!("{}m", 50 * (1 + rng.below(8)))),
        mem_request: SharedString::from(format!("{}Mi", 64 * (1 + rng.below(16)))),
        ports: SharedString::from(format!(
            "{}/TCP",
            match rng.below(4) {
                0 => 80,
                1 => 443,
                2 => 8080,
                _ => 9090,
            }
        )),
        revision: SharedString::from(format!("{}", 1 + rng.below(64))),
        health,
        modified: 0,
    }
}

pub fn synthetic_rows(count: usize, seed: u64) -> Vec<Row> {
    let mut rng = Lcg::new(seed);
    (0..count).map(|_| random_row(&mut rng)).collect()
}

/// Simulate a watch event by changing only the fields that update.
pub fn mutate_row(row: &mut Row, rng: &mut Lcg, frame: u64) {
    let replicas = 1 + rng.below(6);
    let ready = rng.below(replicas + 1);
    row.health = rollout(replicas, ready);
    row.ready = SharedString::from(format!("{ready}/{replicas}"));
    row.status = status_text(row.health);
    row.up_to_date = SharedString::from(format!("{replicas}"));
    row.available = SharedString::from(format!("{ready}"));
    row.restarts = SharedString::from(format!("{}", rng.below(60)));
    row.age = SharedString::from(format_age(rng.below(400 * 86_400)));
    row.revision = SharedString::from(format!("{}", 1 + rng.below(128)));
    row.modified = frame;
}
