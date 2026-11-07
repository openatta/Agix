//! Agix and DeepSeek Harness on the same tasks, the same model and the same
//! kind of recording proxy: what each spends, how long it takes, and whether
//! what it built actually runs.
//!
//! Five tasks, each verified by running the result against inputs the model
//! never saw:
//!
//! - `stats` — short: one small program from a precise spec.
//! - `inventory` — long-horizon: fix three bugs across a package so its test
//!   script passes, then add a CLI; the test script is restored before
//!   checking, so editing it buys nothing.
//! - `biglog` — long input: answer questions about a ~1.4 MB access log; a
//!   harness that pulls the file into the conversation pays for it here.
//! - `shop` — the everyday shape: two turns in one session on an unfamiliar
//!   multi-module project (`tests/fixtures/compare_dsh/shop`). First a bug
//!   report to locate and fix, then a feature request that builds on it,
//!   checked against an independent reference over random carts.
//! - `audit` — a workflow: 24 modules in four packages, six of which break
//!   their own docstring. Fan the review out, one sub-agent per package, fix
//!   what is broken and list it; checked by a hidden contract suite, that the
//!   correct modules are untouched, and that the list is exactly right.
//!
//! Token counts are what DeepSeek reported on each response, read off the
//! proxy, so both sides are measured the same way. Wall time for DSH includes
//! its process start; Agix's daemon is already up. Not part of the `live_*`
//! glob: it needs `AGIX_DSH_BIN` pointing at a `dsh` launcher
//! (`npm i @deepseek-ai/dsh`). `AGIX_COMPARE_CASES=stats,biglog` runs a
//! subset; `AGIX_COMPARE_REPEAT=3` runs each case that many times and reports
//! medians. A summary goes to stderr and to `tests/output/compare_dsh/`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use live::{
    is_pro_model, Exchange, LiveConfig, LiveProvider, LiveRun, Mode, ModelEndpoint, Tokens,
};
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug)]
enum Case {
    Stats,
    Inventory,
    BigLog,
    Shop,
    Audit,
}

impl Case {
    const ALL: [Case; 5] = [
        Case::Stats,
        Case::Inventory,
        Case::BigLog,
        Case::Shop,
        Case::Audit,
    ];

    fn name(self) -> &'static str {
        match self {
            Case::Stats => "stats",
            Case::Inventory => "inventory",
            Case::BigLog => "biglog",
            Case::Shop => "shop",
            Case::Audit => "audit",
        }
    }

    /// One user message per turn, all in the same session.
    fn prompts(self) -> &'static [&'static str] {
        match self {
            Case::Stats => &[STATS_PROMPT],
            Case::Inventory => &[INVENTORY_PROMPT],
            Case::BigLog => &[BIGLOG_PROMPT],
            Case::Shop => &SHOP_PROMPTS,
            Case::Audit => &[AUDIT_PROMPT],
        }
    }

    fn setup(self, project: &Path, seed: u64) -> anyhow::Result<()> {
        match self {
            Case::Stats => Ok(()),
            Case::Inventory => {
                for (path, body) in INVENTORY_FILES {
                    write(project, path, body)?;
                }
                Ok(())
            }
            Case::BigLog => write(project, "logs/access.log", &BigLog::new(seed).text),
            Case::Shop => copy_tree(&shop_fixture(), project),
            Case::Audit => copy_tree(&audit_fixture(), project),
        }
    }

    /// What is wrong with what the harness left behind; empty when nothing is.
    fn verify(self, project: &Path, seed: u64) -> Vec<String> {
        let result = match self {
            Case::Stats => verify_stats(project, seed),
            Case::Inventory => verify_inventory(project, seed),
            Case::BigLog => verify_biglog(project, seed),
            Case::Shop => verify_shop(project, seed),
            Case::Audit => verify_audit(project),
        };
        result.unwrap_or_else(|e| vec![format!("verifier could not run: {e:#}")])
    }
}

const STATS_PROMPT: &str = "\
Write a Python 3 program `stats.py` in the current directory, standard library only. \
Usage: `python3 stats.py <file>`.

The file holds one integer per line; integers may be negative. Blank lines, and lines whose \
first non-space character is `#`, are ignored. Whitespace around a number is ignored.

- If every line is valid and there is at least one number, print exactly one line \
`count=<n> sum=<s> min=<a> max=<b> median=<m>` and exit 0. For an even count the median is the \
mean of the two middle values; print it as an integer when it is whole, otherwise with exactly \
one decimal.
- If there are no numbers, print exactly `count=0` and exit 0.
- If a line is not an integer, print `bad line <k>: <that line, stripped>` to stderr for the \
first such line (k counts every line of the file from 1, ignored ones included), print nothing \
to stdout, and exit 2.

Test it yourself before you finish, then reply DONE.";

const INVENTORY_PROMPT: &str = "\
This project is a small Python package `inventory/` with a test script `run_tests.py`.

1. `python3 run_tests.py` fails. Fix the bugs in the package so every test passes. Do not \
change `run_tests.py`.
2. Add a command-line entry point, run as `python3 -m inventory <file.csv>`. Each non-blank line \
of the file is one operation, applied in order to one `Store`:
   - `add,<name>,<qty>,<unit_price_cents>`
   - `remove,<name>,<qty>`
   A line that cannot be applied — unknown operation, wrong number of fields, a quantity or \
price that is not a positive integer, removing an unknown item or more than is in stock — \
prints `ERROR line <n>: <reason>` to stderr (n counts every line from 1, blank ones included) \
and is skipped; processing continues. After the last line, print the lines of \
`inventory.report.format_report(store)`, then `TOTAL <dollars>` where dollars is \
`inventory.report.dollars` of the store's total value. Exit 1 if any line was an error, \
otherwise 0.

Test both before you finish, then reply DONE.";

const BIGLOG_PROMPT: &str = "\
`logs/access.log` is a web server access log, one request per line, in the form \
`<ip> - - [<time>] \"<METHOD> <path> HTTP/1.1\" <status> <bytes>`.

Write `answer.json` containing a JSON object with:
- `top_5xx`: the three client IPs with the most responses whose status is 500–599, most first, \
ties broken by IP in ascending string order;
- `upload_bytes`: the total bytes of successful (status 200–299) `POST /api/upload` requests;
- `lines`: the number of lines in the file.

Reply DONE when the file is written.";

const SHOP_PROMPTS: [&str; 2] = [
    "\
Bug report from support: a customer bought two copies of \"Cooking for Two\" and a coffee \
machine with coupon BOOKS20, and got 20% off the coffee machine as well. BOOKS20 is meant to \
take 20% off the book items only. Find the cause in this project and fix it, and add a \
regression test to `tests/run_tests.py`. Reply DONE when the tests pass.",
    "\
Marketing wants a new coupon code `FREESHIP`: it takes nothing off the items, but shipping is \
free for that order whatever its size. It has to work from the command line \
(`python3 -m shop cart.csv --coupon FREESHIP`) and, like the other codes, regardless of case. \
Add a test for it too. Reply DONE when the tests pass.",
];

const AUDIT_PROMPT: &str = "\
This repository has a small library under `lib/` in four packages — billing, inventory, text \
and dates — six modules each. Every module's docstring is its contract. Audit the whole \
library: find every module whose code breaks its own docstring and fix the code, changing \
nothing else. Split the work: review the four packages in parallel, one sub-agent per \
package, then merge what they report. Finally write `findings.json` in the project root: a \
sorted JSON array of the paths of the modules you fixed, like `[\"lib/billing/tax.py\"]`. \
Reply DONE when it is written.";

/// The modules the `audit` fixture breaks; every other module is correct.
const AUDIT_BROKEN: [&str; 6] = [
    "lib/billing/discount.py",
    "lib/billing/split.py",
    "lib/dates/age.py",
    "lib/inventory/bins.py",
    "lib/text/slug.py",
    "lib/text/truncate.py",
];

const INVENTORY_FILES: [(&str, &str); 5] = [
    (
        "inventory/__init__.py",
        "from .models import Item\nfrom .store import Store\n",
    ),
    (
        "inventory/models.py",
        r#"from dataclasses import dataclass


@dataclass
class Item:
    name: str
    qty: int
    unit_price_cents: int

    def value_cents(self):
        return self.qty * self.unit_price_cents
"#,
    ),
    (
        "inventory/store.py",
        r#"from .models import Item


class Store:
    """Items by name. Adding a name that is already stocked adds to its
    quantity and keeps its original price; an item whose quantity reaches
    zero is no longer stocked."""

    def __init__(self):
        self._items = []

    def add(self, name, qty, unit_price_cents):
        if qty <= 0:
            raise ValueError("qty must be positive")
        self._items.append(Item(name, qty, unit_price_cents))

    def remove(self, name, qty):
        item = self.get(name)
        if item is None:
            raise KeyError(name)
        if qty > item.qty:
            raise ValueError("insufficient " + name)
        item.qty -= qty

    def get(self, name):
        for item in self._items:
            if item.name == name:
                return item
        return None

    def items(self):
        return list(self._items)

    def total_value_cents(self):
        return sum(i.value_cents() for i in self._items)
"#,
    ),
    (
        "inventory/report.py",
        r#"def dollars(cents):
    return "%d.%02d" % (cents // 100, cents % 100)


def format_report(store):
    """One line per stocked item, by name."""
    lines = []
    for item in sorted(store.items(), key=lambda i: i.qty):
        lines.append("%-12s%5d%10s" % (item.name, item.qty, dollars(item.value_cents())))
    return lines
"#,
    ),
    ("run_tests.py", RUN_TESTS),
];

const RUN_TESTS: &str = r#"import sys

from inventory import Store
from inventory.report import dollars, format_report

failures = 0


def check(name, cond):
    global failures
    if not cond:
        failures += 1
        print("FAIL", name)


s = Store()
s.add("widget", 3, 250)
s.add("widget", 2, 999)
check("add merges same name", len(s.items()) == 1 and s.get("widget").qty == 5)
check("merge keeps first price", s.get("widget").unit_price_cents == 250)
s.add("bolt", 10, 15)
s.remove("bolt", 10)
check("remove to zero drops item", s.get("bolt") is None)
try:
    s.remove("widget", 6)
    check("over-remove raises", False)
except ValueError:
    pass
s.add("anchor", 1, 1234)
check("report sorted by name", [l.split()[0] for l in format_report(s)] == ["anchor", "widget"])
check("dollars", dollars(5) == "0.05" and dollars(123456) == "1234.56")
check("total", s.total_value_cents() == 5 * 250 + 1234)
print("OK" if failures == 0 else "%d failed" % failures)
sys.exit(1 if failures else 0)
"#;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/compare_dsh")
}

fn shop_fixture() -> PathBuf {
    fixtures().join("shop")
}

fn audit_fixture() -> PathBuf {
    fixtures().join("audit")
}

fn copy_tree(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "__pycache__" {
            continue;
        }
        let target = to.join(&name);
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn write(project: &Path, relative: &str, body: &str) -> anyhow::Result<()> {
    let path = project.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, body)?;
    Ok(())
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn int(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }
}

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn python(project: &Path, args: &[&str]) -> anyhow::Result<Ran> {
    let out = std::process::Command::new("python3")
        .args(args)
        .current_dir(project)
        .stdin(Stdio::null())
        .output()?;
    Ok(Ran {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn verify_stats(project: &Path, seed: u64) -> anyhow::Result<Vec<String>> {
    let mut rng = Lcg(seed ^ 0x5747);
    let mut failures = Vec::new();
    let check_dir = tempfile::tempdir()?;
    let program = project.join("stats.py");
    anyhow::ensure!(program.exists(), "stats.py was not written");
    let program = program.to_string_lossy().into_owned();

    let mut cases: Vec<(String, Vec<String>)> = Vec::new();
    for count in [rng.int(5, 12) * 2 + 1, rng.int(5, 12) * 2, 2] {
        let mut lines = vec!["# generated".to_string()];
        for _ in 0..count {
            let n = rng.int(-1000, 1000);
            if rng.below(4) == 0 {
                lines.push(String::new());
            }
            lines.push(if rng.below(3) == 0 {
                format!("  {n} ")
            } else {
                n.to_string()
            });
        }
        cases.push((format!("{count} numbers"), lines));
    }
    let mut halves = vec!["3".to_string(), "-6".to_string()];
    halves.insert(1, "   # a comment".into());
    cases.push(("half median".into(), halves));

    for (label, lines) in &cases {
        let path = check_dir.path().join("in.txt");
        std::fs::write(&path, lines.join("\n") + "\n")?;
        let mut nums: Vec<i64> = lines
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.parse().unwrap())
            .collect();
        nums.sort();
        let n = nums.len();
        let mid = if n % 2 == 1 {
            nums[n / 2].to_string()
        } else {
            let s = nums[n / 2 - 1] + nums[n / 2];
            if s % 2 == 0 {
                (s / 2).to_string()
            } else {
                format!("{:.1}", s as f64 / 2.0)
            }
        };
        let want = format!(
            "count={n} sum={} min={} max={} median={mid}",
            nums.iter().sum::<i64>(),
            nums[0],
            nums[n - 1]
        );
        let ran = python(project, &[&program, &path.to_string_lossy()])?;
        if ran.code != Some(0) || ran.stdout.trim() != want {
            failures.push(format!(
                "stats {label}: want `{want}` exit 0, got `{}` exit {:?}",
                ran.stdout.trim(),
                ran.code
            ));
        }
    }

    let path = check_dir.path().join("empty.txt");
    std::fs::write(&path, "# nothing\n\n   \n")?;
    let ran = python(project, &[&program, &path.to_string_lossy()])?;
    if ran.code != Some(0) || ran.stdout.trim() != "count=0" {
        failures.push(format!(
            "stats empty: got `{}` exit {:?}",
            ran.stdout.trim(),
            ran.code
        ));
    }

    let path = check_dir.path().join("bad.txt");
    std::fs::write(&path, "4\n# x\n\n 7 \n  12abc  \nnope\n")?;
    let ran = python(project, &[&program, &path.to_string_lossy()])?;
    if ran.code != Some(2)
        || !ran.stdout.trim().is_empty()
        || ran.stderr.trim() != "bad line 5: 12abc"
    {
        failures.push(format!(
            "stats bad line: want stderr `bad line 5: 12abc` exit 2, got stdout `{}` stderr `{}` exit {:?}",
            ran.stdout.trim(),
            ran.stderr.trim(),
            ran.code
        ));
    }
    Ok(failures)
}

fn dollars(cents: u64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

fn verify_inventory(project: &Path, seed: u64) -> anyhow::Result<Vec<String>> {
    let mut failures = Vec::new();
    write(project, "run_tests.py", RUN_TESTS)?;
    let ran = python(project, &["run_tests.py"])?;
    if ran.code != Some(0) {
        failures.push(format!(
            "run_tests.py (restored) exit {:?}: {}",
            ran.code,
            ran.stdout.trim()
        ));
    }

    let k = seed % 90 + 10;
    let [gear, bolt, axle, zinc, cam, disk] =
        ["gear", "bolt", "axle", "zinc", "cam", "disk"].map(|n| format!("{n}{k}"));
    let csv = format!(
        "add,{gear},4,250\nadd,{bolt},10,15\nadd,{gear},1,999\nremove,{bolt},3\n\
         remove,{bolt},8\nadd,{axle},2,1234\nremove,{zinc},1\nadd,{cam},x,10\n\
         remove,{axle},2\n\nadd,{disk},3,5\n"
    );
    write(project, "check/ops.csv", &csv)?;
    let mut want: Vec<String> = [(&bolt, 7u64, 15u64), (&disk, 3, 5), (&gear, 5, 250)]
        .iter()
        .map(|(name, qty, price)| format!("{name:<12}{qty:>5}{:>10}", dollars(qty * price)))
        .collect();
    want.push(format!("TOTAL {}", dollars(7 * 15 + 3 * 5 + 5 * 250)));
    let ran = python(project, &["-m", "inventory", "check/ops.csv"])?;
    let got: Vec<&str> = ran.stdout.lines().collect();
    if got != want.iter().map(String::as_str).collect::<Vec<_>>() {
        failures.push(format!(
            "cli stdout: want {want:?}, got {got:?} (stderr {:?})",
            ran.stderr.trim()
        ));
    }
    let error_lines: Vec<String> = ran
        .stderr
        .lines()
        .filter_map(|l| l.strip_prefix("ERROR line "))
        .filter_map(|l| l.split(':').next().map(str::to_string))
        .collect();
    if error_lines != ["5", "7", "8"] {
        failures.push(format!(
            "cli errors: want lines [5, 7, 8], got {error_lines:?} from {:?}",
            ran.stderr.trim()
        ));
    }
    if ran.code != Some(1) {
        failures.push(format!("cli with errors: want exit 1, got {:?}", ran.code));
    }

    write(project, "check/clean.csv", &format!("add,{cam},2,50\n"))?;
    let ran = python(project, &["-m", "inventory", "check/clean.csv"])?;
    let want = format!("{cam:<12}{:>5}{:>10}\nTOTAL 1.00", 2, "1.00");
    if ran.code != Some(0) || ran.stdout.trim_end() != want {
        failures.push(format!(
            "cli clean: want `{want}` exit 0, got `{}` exit {:?}",
            ran.stdout.trim_end(),
            ran.code
        ));
    }
    Ok(failures)
}

struct BigLog {
    text: String,
    top_5xx: Vec<String>,
    upload_bytes: u64,
    lines: usize,
}

impl BigLog {
    const LINES: usize = 20_000;

    fn new(seed: u64) -> Self {
        let mut rng = Lcg(seed ^ 0xb16);
        let ips: Vec<String> = (0..240)
            .map(|_| {
                format!(
                    "10.{}.{}.{}",
                    rng.below(256),
                    rng.below(256),
                    rng.below(254) + 1
                )
            })
            .collect();
        let paths = [
            "/",
            "/index.html",
            "/api/items",
            "/api/items/42",
            "/api/upload",
            "/static/app.js",
            "/login",
            "/api/search?q=x",
        ];
        let mut text = String::with_capacity(Self::LINES * 80);
        let mut fives: BTreeMap<&str, u32> = BTreeMap::new();
        let mut upload_bytes = 0;
        for i in 0..Self::LINES {
            let ip = &ips[(rng.below(ips.len() as u64) * rng.below(ips.len() as u64)
                / ips.len() as u64) as usize];
            let path = paths[rng.below(paths.len() as u64) as usize];
            let method = if path == "/api/upload" || rng.below(10) == 0 {
                "POST"
            } else {
                "GET"
            };
            let status = match rng.below(100) {
                0..=2 => [500, 502, 503, 504][rng.below(4) as usize],
                3..=6 => 404,
                7 => 413,
                8..=10 => 201,
                11..=12 => 304,
                _ => 200,
            };
            let bytes = rng.below(90_000) + 120;
            if (500..600).contains(&status) {
                *fives.entry(ip).or_default() += 1;
            }
            if method == "POST" && path == "/api/upload" && (200..300).contains(&status) {
                upload_bytes += bytes;
            }
            text.push_str(&format!(
                "{ip} - - [06/Nov/2026:{:02}:{:02}:{:02} +0000] \"{method} {path} HTTP/1.1\" {status} {bytes}\n",
                (i / 3600) % 24,
                (i / 60) % 60,
                i % 60
            ));
        }
        let mut ranked: Vec<(&str, u32)> = fives.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        Self {
            text,
            top_5xx: ranked
                .iter()
                .take(3)
                .map(|(ip, _)| ip.to_string())
                .collect(),
            upload_bytes,
            lines: Self::LINES,
        }
    }
}

fn verify_biglog(project: &Path, seed: u64) -> anyhow::Result<Vec<String>> {
    let want = BigLog::new(seed);
    let text = std::fs::read_to_string(project.join("answer.json"))
        .map_err(|e| anyhow::anyhow!("answer.json: {e}"))?;
    let got: Value = serde_json::from_str(&text)?;
    let mut failures = Vec::new();
    if got["top_5xx"] != json!(want.top_5xx) {
        failures.push(format!(
            "top_5xx: want {:?}, got {}",
            want.top_5xx, got["top_5xx"]
        ));
    }
    if got["upload_bytes"].as_u64() != Some(want.upload_bytes) {
        failures.push(format!(
            "upload_bytes: want {}, got {}",
            want.upload_bytes, got["upload_bytes"]
        ));
    }
    if got["lines"].as_u64() != Some(want.lines as u64) {
        failures.push(format!("lines: want {}, got {}", want.lines, got["lines"]));
    }
    Ok(failures)
}

const SHOP_SKUS: [&str; 12] = [
    "BK-101", "BK-102", "BK-103", "BK-104", "KT-201", "KT-202", "KT-203", "FD-301", "FD-302",
    "FD-303", "TY-401", "TY-402",
];

fn verify_shop(project: &Path, seed: u64) -> anyhow::Result<Vec<String>> {
    let mut failures = Vec::new();
    let pristine = project.join(".pristine/run_tests.py");
    std::fs::create_dir_all(pristine.parent().unwrap())?;
    std::fs::copy(shop_fixture().join("tests/run_tests.py"), &pristine)?;
    for script in [".pristine/run_tests.py", "tests/run_tests.py"] {
        let ran = python(project, &[script])?;
        if ran.code != Some(0) {
            failures.push(format!(
                "{script} exited {:?}: {}",
                ran.code,
                ran.stdout.trim()
            ));
        }
    }

    let carts = tempfile::tempdir()?;
    let reference = fixtures()
        .join("reference.py")
        .to_string_lossy()
        .into_owned();
    let pristine_shop = shop_fixture().to_string_lossy().into_owned();
    let mut rng = Lcg(seed ^ 0x5409);
    let coupons = [
        None,
        Some("SAVE10"),
        Some("FLAT5"),
        Some("BOOKS20"),
        Some("books20"),
        Some("FREESHIP"),
        Some("freeShip"),
    ];
    let mut wrong = 0;
    for i in 0..42 {
        let mut body = String::from("# generated\n");
        for _ in 0..rng.int(1, 4) {
            let sku = SHOP_SKUS[rng.below(SHOP_SKUS.len() as u64) as usize];
            body.push_str(&format!("{sku},{}\n", rng.int(1, 3)));
        }
        let cart = carts.path().join(format!("cart{i}.csv"));
        std::fs::write(&cart, &body)?;
        let cart = cart.to_string_lossy().into_owned();
        let coupon = coupons[i % coupons.len()];
        let mut args = vec!["-m", "shop", cart.as_str()];
        let mut want_args = vec![reference.as_str(), pristine_shop.as_str(), cart.as_str()];
        if let Some(code) = coupon {
            args.extend(["--coupon", code]);
            want_args.push(code);
        }
        let got = python(project, &args)?;
        let want = python(project, &want_args)?;
        anyhow::ensure!(want.code == Some(0), "reference failed: {}", want.stderr);
        if got.code != Some(0) || got.stdout != want.stdout {
            wrong += 1;
            if wrong <= 3 {
                failures.push(format!(
                    "cart {body:?} coupon {coupon:?}: want {:?}, got {:?} exit {:?} {}",
                    want.stdout,
                    got.stdout,
                    got.code,
                    got.stderr.trim()
                ));
            }
        }
    }
    if wrong > 3 {
        failures.push(format!("{wrong} of 42 carts priced wrong in all"));
    }

    let cart = carts.path().join("one.csv");
    std::fs::write(&cart, "KT-203,1\n")?;
    let bogus = python(
        project,
        &["-m", "shop", cart.to_str().unwrap(), "--coupon", "BOGUS"],
    )?;
    if bogus.code != Some(2) {
        failures.push(format!("unknown coupon exited {:?}, want 2", bogus.code));
    }
    Ok(failures)
}

/// Every contract holds (`audit_check.py`), the correct modules are untouched,
/// and `findings.json` names exactly the broken ones.
fn verify_audit(project: &Path) -> anyhow::Result<Vec<String>> {
    let mut failures = Vec::new();
    let checker = fixtures().join("audit_check.py");
    let ran = python(
        project,
        &[checker.to_str().unwrap(), project.to_str().unwrap()],
    )?;
    if ran.code != Some(0) {
        failures.extend(ran.stdout.lines().map(|l| format!("contract: {l}")));
        if ran.stdout.trim().is_empty() {
            failures.push(format!(
                "checker exited {:?}: {}",
                ran.code,
                ran.stderr.trim()
            ));
        }
    }

    let fixture = audit_fixture();
    for package in ["billing", "inventory", "text", "dates"] {
        for entry in std::fs::read_dir(fixture.join("lib").join(package))? {
            let path = entry?.path();
            let relative = path
                .strip_prefix(&fixture)?
                .to_string_lossy()
                .replace('\\', "/");
            if !relative.ends_with(".py") || relative.ends_with("__init__.py") {
                continue;
            }
            if AUDIT_BROKEN.contains(&relative.as_str()) {
                continue;
            }
            let normalized = |text: String| {
                text.lines()
                    .map(str::trim_end)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim_end()
                    .to_string()
            };
            let before = normalized(std::fs::read_to_string(&path)?);
            let after = std::fs::read_to_string(project.join(&relative)).map(normalized);
            if after.as_deref().ok() != Some(before.as_str()) {
                failures.push(format!("{relative} was correct and has been changed"));
            }
        }
    }

    match std::fs::read_to_string(project.join("findings.json")) {
        Err(e) => failures.push(format!("findings.json: {e}")),
        Ok(text) => match serde_json::from_str::<Vec<String>>(&text) {
            Err(e) => failures.push(format!("findings.json is not an array of paths: {e}")),
            Ok(found) => {
                let found: std::collections::BTreeSet<String> = found
                    .iter()
                    .map(|p| p.trim_start_matches("./").to_string())
                    .collect();
                let want: std::collections::BTreeSet<String> =
                    AUDIT_BROKEN.iter().map(|p| p.to_string()).collect();
                if found != want {
                    failures.push(format!(
                        "findings.json: missing {:?}, extra {:?}",
                        want.difference(&found).collect::<Vec<_>>(),
                        found.difference(&want).collect::<Vec<_>>()
                    ));
                }
            }
        },
    }
    Ok(failures)
}

#[derive(Debug)]
struct Outcome {
    harness: &'static str,
    case: Case,
    finished: bool,
    wall: Duration,
    calls: usize,
    tool_calls: usize,
    tokens: Tokens,
    first_prompt: u64,
    models: Vec<String>,
    shape: Value,
    first_request: Value,
    per_call: Vec<[u64; 3]>,
    failures: Vec<String>,
    note: String,
}

impl Outcome {
    fn from_exchanges(harness: &'static str, case: Case, exchanges: &[Exchange]) -> Self {
        let calls: Vec<&Exchange> = exchanges
            .iter()
            .filter(|e| e.request.path.ends_with("/v1/messages"))
            .collect();
        let mut tokens = Tokens::default();
        for e in &calls {
            tokens += e.tokens();
        }
        let mut models: Vec<String> = calls
            .iter()
            .filter_map(|e| e.request.body["model"].as_str().map(str::to_string))
            .collect();
        models.sort();
        models.dedup();
        let main = calls.iter().find(|e| {
            e.request.body["tools"]
                .as_array()
                .is_some_and(|t| !t.is_empty())
        });
        let shape = main
            .map(|e| {
                let body = &e.request.body;
                json!({
                    "tools": body["tools"].as_array().map_or(0, Vec::len),
                    "tools_bytes": body["tools"].to_string().len(),
                    "system_bytes": body["system"].to_string().len(),
                    "max_tokens": body["max_tokens"],
                    "thinking": body["thinking"],
                    "output_config": body["output_config"],
                })
            })
            .unwrap_or(Value::Null);
        Self {
            harness,
            case,
            finished: false,
            wall: Duration::ZERO,
            calls: calls.len(),
            tool_calls: 0,
            tokens,
            first_prompt: main.map(|e| e.tokens().prompt()).unwrap_or(0),
            models,
            shape,
            first_request: main.map(|e| e.request.body.clone()).unwrap_or(Value::Null),
            per_call: calls
                .iter()
                .map(|e| {
                    let t = e.tokens();
                    [t.input + t.cache_write, t.cache_read, t.output]
                })
                .collect(),
            failures: Vec::new(),
            note: String::new(),
        }
    }

    fn passed(&self) -> bool {
        self.finished && self.failures.is_empty()
    }

    fn to_json(&self) -> Value {
        json!({
            "harness": self.harness,
            "case": self.case.name(),
            "passed": self.passed(),
            "finished": self.finished,
            "wall_secs": self.wall.as_secs_f64(),
            "model_calls": self.calls,
            "tool_calls": self.tool_calls,
            "input_tokens": self.tokens.input,
            "cache_read_tokens": self.tokens.cache_read,
            "cache_write_tokens": self.tokens.cache_write,
            "output_tokens": self.tokens.output,
            "first_tool_call_prompt_tokens": self.first_prompt,
            "models": self.models,
            "request_shape": self.shape,
            "per_call_uncached_cached_output": self.per_call,
            "failures": self.failures,
            "note": self.note,
        })
    }
}

async fn run_agix(case: Case, seed: u64) -> anyhow::Result<Outcome> {
    let mut run =
        LiveRun::start(&format!("agix-{}", case.name()), json!({}), Mode::InProcess).await?;
    case.setup(&run.project, seed)?;
    let session = run.session().await?;
    let started = Instant::now();
    let mut tool_calls = 0;
    let mut error = None;
    for prompt in case.prompts() {
        match run.turn(&session, prompt).await {
            Ok(events) => tool_calls += events.tool_uses.len(),
            Err(e) => {
                error = Some(format!("{e:#}"));
                break;
            }
        }
    }
    let wall = started.elapsed();
    let mut outcome = Outcome::from_exchanges("agix", case, &run.provider.exchanges());
    outcome.wall = wall;
    outcome.tool_calls = tool_calls;
    match error {
        None => outcome.finished = true,
        Some(e) => outcome.note = e,
    }
    outcome.failures = case.verify(&run.project, seed);
    run.finish().await;
    Ok(outcome)
}

async fn run_dsh(bin: &Path, case: Case, seed: u64) -> anyhow::Result<Outcome> {
    let config = LiveConfig::load()?;
    let timeout = config.turn_timeout;
    let provider = LiveProvider::start(config).await?;
    let dir = tempfile::tempdir()?;
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project)?;
    case.setup(&project, seed)?;

    let started = Instant::now();
    let mut session_id: Option<String> = None;
    let mut tool_calls = 0;
    let mut error = None;
    for prompt in case.prompts() {
        let mut command = tokio::process::Command::new(bin);
        command.args(["--profile", "headless", "--json"]);
        if let Some(id) = &session_id {
            command.args(["--session-id", id]);
        }
        let child = command
            .arg(prompt)
            .current_dir(&project)
            .env("DSH_HOME", dir.path().join("dsh-home"))
            .env("DEEPSEEK_API_KEY", "live-proxy-swaps-this")
            .env(
                "DEEPSEEK_BASE_URL",
                provider.base_url().as_str().trim_end_matches('/'),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                error = Some(format!("dsh did not run: {e}"));
                break;
            }
            Err(_) => {
                error = Some(format!("dsh still running after {timeout:?}"));
                break;
            }
        };
        let events: Vec<Value> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        tool_calls += events.iter().filter(|e| e["type"] == "tool_call").count();
        if session_id.is_none() {
            session_id = events
                .iter()
                .find(|e| e["type"] == "session")
                .and_then(|e| e["sessionId"].as_str())
                .map(str::to_string);
        }
        let reason = events
            .iter()
            .rev()
            .find(|e| e["type"] == "final")
            .map(|e| e["reason"]["kind"].clone())
            .unwrap_or(Value::Null);
        if !out.status.success() || reason != "completed" {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let tail: String = stderr.lines().rev().take(8).collect::<Vec<_>>().join(" | ");
            error = Some(format!(
                "exit {:?}, final reason {reason}: {tail}",
                out.status.code()
            ));
            break;
        }
    }
    let wall = started.elapsed();

    let mut outcome = Outcome::from_exchanges("dsh", case, &provider.exchanges());
    outcome.wall = wall;
    outcome.tool_calls = tool_calls;
    match error {
        None => outcome.finished = true,
        Some(e) => outcome.note = e,
    }
    outcome.failures = case.verify(&project, seed);
    Ok(outcome)
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

/// Every run of one harness on one case, reduced to medians.
struct Summary<'a> {
    case: Case,
    harness: &'static str,
    runs: Vec<&'a Outcome>,
}

impl Summary<'_> {
    fn group(outcomes: &[Outcome]) -> Vec<Summary<'_>> {
        let mut groups: Vec<Summary<'_>> = Vec::new();
        for o in outcomes {
            match groups
                .iter_mut()
                .find(|g| g.case.name() == o.case.name() && g.harness == o.harness)
            {
                Some(g) => g.runs.push(o),
                None => groups.push(Summary {
                    case: o.case,
                    harness: o.harness,
                    runs: vec![o],
                }),
            }
        }
        groups
    }

    fn med(&self, f: impl Fn(&Outcome) -> f64) -> f64 {
        median(self.runs.iter().map(|o| f(o)).collect())
    }

    fn passed(&self) -> usize {
        self.runs.iter().filter(|o| o.passed()).count()
    }

    fn to_json(&self) -> Value {
        json!({
            "case": self.case.name(),
            "harness": self.harness,
            "runs": self.runs.len(),
            "passed": self.passed(),
            "median": {
                "wall_secs": self.med(|o| o.wall.as_secs_f64()),
                "model_calls": self.med(|o| o.calls as f64),
                "tool_calls": self.med(|o| o.tool_calls as f64),
                "input_tokens": self.med(|o| o.tokens.input as f64),
                "cache_read_tokens": self.med(|o| o.tokens.cache_read as f64),
                "prompt_tokens": self.med(|o| o.tokens.prompt() as f64),
                "output_tokens": self.med(|o| o.tokens.output as f64),
                "first_tool_call_prompt_tokens": self.med(|o| o.first_prompt as f64),
            },
        })
    }
}

fn print_table(outcomes: &[Outcome]) {
    eprintln!(
        "\n{:<10} {:<5} {:>5} {:>7} {:>6} {:>6} {:>9} {:>10} {:>9} {:>8} {:>8}",
        "case",
        "who",
        "pass",
        "wall s",
        "calls",
        "tools",
        "input",
        "cache_read",
        "prompt",
        "output",
        "1st tool"
    );
    for g in Summary::group(outcomes) {
        eprintln!(
            "{:<10} {:<5} {:>5} {:>7.1} {:>6.1} {:>6.1} {:>9.0} {:>10.0} {:>9.0} {:>8.0} {:>8.0}",
            g.case.name(),
            g.harness,
            format!("{}/{}", g.passed(), g.runs.len()),
            g.med(|o| o.wall.as_secs_f64()),
            g.med(|o| o.calls as f64),
            g.med(|o| o.tool_calls as f64),
            g.med(|o| o.tokens.input as f64),
            g.med(|o| o.tokens.cache_read as f64),
            g.med(|o| o.tokens.prompt() as f64),
            g.med(|o| o.tokens.output as f64),
            g.med(|o| o.first_prompt as f64),
        );
    }
    eprintln!("(medians over runs)");
    for o in outcomes {
        eprintln!(
            "  {} {}: {} wall {:.1}s calls {} tools {} prompt {} output {}; models {:?}; request {}",
            o.case.name(),
            o.harness,
            if o.passed() { "pass" } else { "FAIL" },
            o.wall.as_secs_f64(),
            o.calls,
            o.tool_calls,
            o.tokens.prompt(),
            o.tokens.output,
            o.models,
            o.shape
        );
        for f in &o.failures {
            eprintln!("    - {f}");
        }
        if !o.note.is_empty() {
            eprintln!("    note: {}", o.note);
        }
    }
}

fn dsh_bin() -> anyhow::Result<PathBuf> {
    let bin = std::env::var("AGIX_DSH_BIN").map_err(|_| {
        anyhow::anyhow!("set AGIX_DSH_BIN to a `dsh` launcher (npm i @deepseek-ai/dsh)")
    })?;
    let bin = PathBuf::from(bin);
    anyhow::ensure!(
        bin.exists(),
        "AGIX_DSH_BIN {} does not exist",
        bin.display()
    );
    Ok(bin)
}

#[tokio::test(flavor = "multi_thread")]
async fn agix_and_dsh_on_the_same_tasks() -> anyhow::Result<()> {
    let bin = dsh_bin()?;
    let wanted = std::env::var("AGIX_COMPARE_CASES").ok();
    let cases: Vec<Case> = Case::ALL
        .into_iter()
        .filter(|c| {
            wanted
                .as_deref()
                .is_none_or(|w| w.split(',').any(|n| n.trim() == c.name()))
        })
        .collect();
    let repeat: u64 = std::env::var("AGIX_COMPARE_REPEAT")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1)
        .max(1);
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as u64;

    let mut outcomes = Vec::new();
    for round in 0..repeat {
        for &case in &cases {
            let seed = seed.wrapping_add(round);
            let (agix, dsh) = tokio::join!(run_agix(case, seed), run_dsh(&bin, case, seed));
            outcomes.push(agix?);
            outcomes.push(dsh?);
        }
    }
    print_table(&outcomes);

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../output/compare_dsh");
    std::fs::create_dir_all(&dir)?;
    let report = json!({
        "seed": seed,
        "repeat": repeat,
        "summary": Summary::group(&outcomes).iter().map(Summary::to_json).collect::<Vec<_>>(),
        "outcomes": outcomes.iter().map(Outcome::to_json).collect::<Vec<_>>(),
    });
    let file = dir.join(format!("{seed}.json"));
    std::fs::write(&file, serde_json::to_string_pretty(&report)?)?;
    for g in Summary::group(&outcomes) {
        let request = dir.join(format!(
            "{seed}-{}-{}.request.json",
            g.case.name(),
            g.harness
        ));
        std::fs::write(
            request,
            serde_json::to_string_pretty(&g.runs[0].first_request)?,
        )?;
    }
    eprintln!(
        "\nreport: {} (first tool-carrying request of each beside it)",
        file.display()
    );

    for o in &outcomes {
        assert!(
            o.models.iter().all(|m| !is_pro_model(m)),
            "{} asked for a Pro model: {:?}",
            o.harness,
            o.models
        );
    }
    let agix_failed: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| o.harness == "agix" && !o.passed())
        .collect();
    assert!(agix_failed.is_empty(), "agix failed: {agix_failed:#?}");
    Ok(())
}
