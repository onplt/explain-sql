//! The parser returns a plan or an error, but never panics or hangs,
//! whatever it is given.

mod common;

use common::{corpus, fixtures, plan_path, read};
use explainsql_core::parse;

/// A small deterministic generator (xorshift64*), so failures reproduce.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Bytes that matter to the parsers.
const INTERESTING: &[u8] = b" \n\t()[]{}\":,.=-+>|'0123456789ex\xff";

fn mutate(sample: &str, random: &mut Random) -> String {
    let mut bytes = sample.as_bytes().to_vec();
    for _ in 0..1 + random.below(6) {
        if bytes.is_empty() {
            break;
        }
        let at = random.below(bytes.len());
        match random.below(5) {
            0 => bytes[at] = INTERESTING[random.below(INTERESTING.len())],
            1 => {
                bytes.remove(at);
            }
            2 => bytes.insert(at, INTERESTING[random.below(INTERESTING.len())]),
            3 => {
                let end = (at + 1 + random.below(200)).min(bytes.len());
                bytes.drain(at..end);
            }
            _ => {
                // Duplicate a slice somewhere else: repeated lines and keys.
                let end = (at + 1 + random.below(120)).min(bytes.len());
                let slice = bytes[at..end].to_vec();
                let to = random.below(bytes.len());
                bytes.splice(to..to, slice);
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn mangled_plans_never_panic() {
    let mut random = Random(0x9e37_79b9_7f4a_7c15);
    let mut samples: Vec<String> = corpus()
        .into_iter()
        .filter(|(major, _)| [12, 18].contains(major))
        .flat_map(|(major, scenario)| {
            [
                read(&plan_path(major, &scenario, "txt")),
                read(&plan_path(major, &scenario, "json")),
            ]
        })
        .collect();
    for entry in std::fs::read_dir(fixtures().join("inputs")).unwrap() {
        samples.push(read(&entry.unwrap().path()));
    }
    for sample in &samples {
        let step = (sample.len() / 25).max(1);
        for cut in (0..sample.len()).step_by(step) {
            if sample.is_char_boundary(cut) {
                let _ = parse(&sample[..cut]);
            }
        }
        for _ in 0..20 {
            let _ = parse(&mutate(sample, &mut random));
        }
    }
}

#[test]
fn odd_inputs_never_panic() {
    // Indentation grows with depth, so the text grows quadratically.
    let deep_text: String = (0..1_500)
        .map(|depth| {
            format!(
                "{}->  Nested Loop  (cost=0.00..1.00 rows=1 width=4)\n",
                " ".repeat(depth * 6)
            )
        })
        .collect();
    let wide_text: String =
        std::iter::once("Append  (cost=0.00..1.00 rows=1 width=4)\n".to_owned())
            .chain(
                (0..20_000)
                    .map(|_| "  ->  Seq Scan on t  (cost=0.00..1.00 rows=1 width=4)\n".to_owned()),
            )
            .collect();
    let repeated = format!(
        "Seq Scan on t  (cost=0.00..1.00 rows=1 width=4)\n{}",
        "  Filter: (a = 1)\n".repeat(5_000)
    );
    let deep_json = format!("{}1{}", "[".repeat(100_000), "]".repeat(100_000));
    let open_json = "[{\"Plan\": {\"Plans\": [".repeat(10_000);
    let inputs = [
        "->",
        "  ->  ",
        "->  Seq Scan",
        "(cost=)",
        "Seq Scan on t  (cost=1..2 rows=x width=4)",
        "Result  (cost=0.00..0.01 rows=1 width=4)\nWorker 0:  actual rows=1 loops=1",
        "Result  (cost=0.00..0.01 rows=1 width=4)\n  Worker 0:\n    Worker 1:  x\n  Buffers: shared hit=a",
        "Result  (cost=0.00..0.01 rows=1 width=4)\nPlanning:\n  Memory: used=kB\nJIT:\n  Timing: Generation",
        "Result  (cost=0.00..0.01 rows=1 width=4)\nTrigger : time=",
        "Result  (cost=0.00..0.01 rows=1 width=4)\n  InitPlan 1\nSubPlan 1\n      ->",
        "QUERY PLAN\n-----\n(1 row)",
        "-[ RECORD 1 ]-\nQUERY PLAN |",
        "```",
        "duration: 1 ms  plan:",
        "{\"message\": \"duration: 1 ms  plan:\\n\"}",
        "\u{feff}\r\r\r",
        "\"",
        "\"\"",
        "\"QUERY PLAN\"\n\"",
        "\"[\n{\"\"Plan\"\": \"",
        "2026-10-04 17:03:46.512 UTC,\"a,b,\"duration: 1 ms  plan:\n",
        "2026-10-04 17:03:46.512 UTC,,,,,,,,,,,,,\"duration: 1 ms  plan:\nResult  (cost=0.00..0.01 rows=1 width=4)",
        "Result  (cost=0.00..0.01 rows=1 width=4)\nResult  (cost=0.00..0.01 rows=1 width=4)",
        "[{\"Plan\": {\"Node Type\": 1, \"Plans\": [1, \"x\", {\"Workers\": [1]}], \"Output\": [1]}, \"Triggers\": [1], \"JIT\": {\"Timing\": 3}}]",
        &deep_text,
        &wide_text,
        &repeated,
        &deep_json,
        &open_json,
    ];
    for input in inputs {
        let _ = parse(input);
    }
    // The large plans are read completely.
    assert_eq!(parse(&deep_text).unwrap().nodes.len(), 1_500);
    assert_eq!(parse(&wide_text).unwrap().nodes.len(), 20_001);
}
