//! Frames of the viewer on plans from the corpus, at a comfortable size
//! and at 80×24, compared with snapshots.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use explainsql_tui::{App, Background, Depth, Key, Theme};
use ratatui::buffer::Buffer;

fn plan(scenario: &str) -> App {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/pg/16")
        .join(format!("{scenario}.txt"));
    let text = std::fs::read_to_string(&path).unwrap();
    let plan = explainsql_core::parse(&text).unwrap();
    let analysis = explainsql_core::analyze(&plan);
    App::new(plan, analysis)
}

fn theme() -> Theme {
    Theme::new(Background::Dark, Depth::None)
}

/// The characters of a frame, one line per row, without trailing spaces.
fn text(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in 0..area.height {
        let mut line = String::new();
        let mut x = 0;
        while x < area.width {
            let cell = &buffer[(x, y)];
            line.push_str(cell.symbol());
            x += 1;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn frame(app: &mut App, width: u16, height: u16) -> String {
    text(&explainsql_tui::render(app, &theme(), width, height))
}

#[test]
fn plans_at_two_sizes() {
    for scenario in [
        "seq_scan_selective",
        "nested_loop_inner_seq_scan",
        "parallel_hash_join",
        "cte_multiple_scans",
        "partition_append_all",
    ] {
        let mut app = plan(scenario);
        insta::assert_snapshot!(format!("{scenario}_120x40"), frame(&mut app, 120, 40));
        insta::assert_snapshot!(format!("{scenario}_80x24"), frame(&mut app, 80, 24));
    }
}

#[test]
fn help_search_and_findings() {
    let mut app = plan("nested_loop_inner_seq_scan");
    app.handle(Key::Char('?'), 10);
    insta::assert_snapshot!("help", frame(&mut app, 100, 30));
    app.handle(Key::Char('?'), 10);
    for c in "/order_items".chars() {
        app.handle(Key::Char(c), 10);
    }
    insta::assert_snapshot!("search_typing", frame(&mut app, 100, 30));
    app.handle(Key::Enter, 10);
    insta::assert_snapshot!("search_found", frame(&mut app, 100, 30));
    app.handle(Key::Tab, 10);
    insta::assert_snapshot!("findings", frame(&mut app, 100, 30));
}

#[test]
fn advice() {
    let mut app = plan("lateral_join_top_n");
    app.handle(Key::Char('i'), 10);
    insta::assert_snapshot!("advice_120x40", frame(&mut app, 120, 40));
    insta::assert_snapshot!("advice_80x24", frame(&mut app, 80, 24));
    let copied = app.handle(Key::Char('c'), 10);
    assert_eq!(
        copied,
        explainsql_tui::Outcome::Copy(
            "CREATE INDEX CONCURRENTLY ON public.orders (customer_id, created_at);".to_owned()
        )
    );
    // Plans with no finding open on the advice.
    let mut app = plan("sort_top_n_heapsort");
    insta::assert_snapshot!("advice_without_findings", frame(&mut app, 100, 30));
}

#[test]
fn view_modes() {
    let mut app = plan("parallel_hash_join");
    app.handle(Key::Char('x'), 10);
    app.handle(Key::Char('w'), 10);
    insta::assert_snapshot!("inclusive_cpu", frame(&mut app, 120, 30));
    app.handle(Key::Char('b'), 10);
    insta::assert_snapshot!("inclusive_buffers", frame(&mut app, 120, 30));
}

/// The decision card: what the planner said when asked why, in the
/// details of the node.
#[test]
fn why_not_card() {
    use explainsql_core::counterfactual::{self, Evaluation, Target};
    let mut app = plan("seq_scan_function_on_column");
    let question = counterfactual::questions(
        &app.plan,
        &app.analysis,
        None,
        &Target::Node(explainsql_core::ir::NodeId(0)),
        false,
    )
    .remove(0);
    let alternative = explainsql_core::parse(
        "Seq Scan on public.orders  (cost=10000000000.00..10000005417.00 rows=1000 width=64)\n  Filter: (date_trunc('day'::text, orders.created_at) = '2024-06-01 00:00:00+00'::timestamp with time zone)",
    )
    .unwrap();
    let answer = counterfactual::answer(
        &app.plan,
        &app.analysis,
        &question,
        &Evaluation {
            chosen: &[],
            alternative: std::slice::from_ref(&alternative),
            with_cost_settings: None,
            cost_settings_runs: &[],
            catalog: None,
        },
    );
    app.analysis.record(vec![answer]);
    insta::assert_snapshot!("why_not_120x50", frame(&mut app, 120, 50));
}

/// A plan of 5,000 nodes that cannot be folded into groups: building the
/// viewer and drawing a frame stay fast enough for typing.
#[test]
fn large_plans_draw_quickly() {
    let mut text = String::from(
        "Append  (cost=0.00..10.00 rows=5000 width=4) (actual time=0.010..90.000 rows=5000 loops=1)\n",
    );
    for n in 0..4999 {
        // Letters rather than digits, so that no two siblings look alike.
        let name: String = format!("{n:04}")
            .chars()
            .map(|digit| char::from(b'a' + digit.to_digit(10).unwrap() as u8))
            .collect();
        text.push_str(&format!(
            "  ->  Seq Scan on t_{name}  (cost=0.00..1.00 rows=1 width=4) (actual time=0.001..0.010 rows=1 loops=1)\n        Filter: (c_{name} = 1)\n"
        ));
    }
    let plan = explainsql_core::parse(&text).unwrap();
    assert_eq!(plan.nodes.len(), 5000);
    let analysis = explainsql_core::analyze(&plan);
    let mut app = App::new(plan, analysis);
    assert_eq!(app.rows().len(), 5000);
    let theme = theme();
    explainsql_tui::render(&mut app, &theme, 120, 40);
    let start = Instant::now();
    let frames = 20;
    for _ in 0..frames {
        app.handle(Key::PageDown, 30);
        explainsql_tui::render(&mut app, &theme, 120, 40);
    }
    let per_frame = start.elapsed() / frames;
    // 16 ms in release builds; debug builds are several times slower.
    let limit = if cfg!(debug_assertions) {
        Duration::from_millis(200)
    } else {
        Duration::from_millis(16)
    };
    eprintln!("{per_frame:?} per frame");
    assert!(per_frame < limit, "{per_frame:?} per frame");
}
