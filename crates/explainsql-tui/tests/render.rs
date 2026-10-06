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

/// The icicle: each node a box under its parent, as wide as the time in
/// it and below it; by estimated cost when the plan has no timing.
#[test]
fn icicle_at_two_sizes() {
    for scenario in [
        "parallel_hash_join",
        "cte_multiple_scans",
        "initplan",
        "partition_append_all",
        "timing_off",
        "delete_fk_trigger",
    ] {
        let mut app = plan(scenario);
        app.handle(Key::Char('F'), 10);
        insta::assert_snapshot!(
            format!("icicle_{scenario}_120x40"),
            frame(&mut app, 120, 40)
        );
        insta::assert_snapshot!(format!("icicle_{scenario}_80x24"), frame(&mut app, 80, 24));
    }
}

/// Moving in the icicle moves the tree's selection, and zoom fills the
/// width with a subtree.
#[test]
fn icicle_moves_and_zooms() {
    use explainsql_core::ir::NodeId;
    let mut app = plan("parallel_hash_join");
    app.handle(Key::Char('F'), 10);
    frame(&mut app, 120, 40);
    assert_eq!(app.selected_node(), NodeId(0));
    // Down to the widest child, then along the row and back up.
    app.handle(Key::Char('j'), 10);
    let child = app.selected_node();
    assert_eq!(app.plan.node(child).parent, Some(NodeId(0)));
    app.handle(Key::Char('j'), 10);
    let grandchild = app.selected_node();
    assert_eq!(app.plan.node(grandchild).parent, Some(child));
    app.handle(Key::Char('k'), 10);
    assert_eq!(app.selected_node(), child);
    // Zoom on it: it takes the whole width, and the title says where.
    app.handle(Key::Enter, 10);
    assert_eq!(app.icicle, Some(child));
    insta::assert_snapshot!("icicle_zoomed", frame(&mut app, 120, 40));
    // Enter on the box zoomed on zooms out; g goes back to the whole plan.
    app.handle(Key::Enter, 10);
    assert_eq!(app.icicle, Some(NodeId(0)));
    app.handle(Key::Char('j'), 10);
    app.handle(Key::Char('g'), 10);
    assert_eq!(app.selected_node(), NodeId(0));
    // F goes back to the tree, on the same node.
    app.handle(Key::Char('j'), 10);
    let node = app.selected_node();
    app.handle(Key::Char('F'), 10);
    assert_eq!(app.icicle, None);
    assert_eq!(app.selected_node(), node);
    // A finding's node too narrow to show is zoomed to.
    let mut app = plan("partition_append_all");
    app.handle(Key::Char('F'), 10);
    let last = *app.plan.root().children.last().unwrap();
    app.reveal(last);
    frame(&mut app, 80, 24);
    assert!(
        explainsql_tui::render(&mut app, &theme(), 80, 24)
            .content()
            .iter()
            .any(|cell| cell.symbol() == "▏")
    );
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

/// The locks overlay, scrolled and closed; L without locks says why.
#[test]
fn locks_overlay() {
    use explainsql_core::locks::{
        self, Capture, Held, QualifiedName, Relation, RelationKind, Stage,
    };
    let mut app = plan("seq_scan_selective");
    app.handle(Key::Char('L'), 10);
    assert!(app.locks.is_none());
    assert!(app.message.as_deref().unwrap().starts_with("Not connected"));

    let relation = |oid, name: &str, kind, scans, enforces| Relation {
        oid,
        schema: "public".to_owned(),
        name: name.to_owned(),
        kind,
        table: QualifiedName::new("public", "orders"),
        root: None,
        scans,
        enforces,
    };
    let held = |oid| Held {
        locktype: "relation".to_owned(),
        relation: Some(oid),
        mode: "AccessShareLock".to_owned(),
        granted: true,
        fastpath: true,
    };
    let capture = Capture {
        stage: Stage::Ran,
        held: vec![held(1), held(2), held(3), held(4)],
        relations: vec![
            relation(1, "orders", RelationKind::Table, None, false),
            relation(2, "orders_pkey", RelationKind::Index, Some(9000), true),
            relation(
                3,
                "orders_customer_id_idx",
                RelationKind::Index,
                Some(120),
                false,
            ),
            relation(4, "orders_note_idx", RelationKind::Index, Some(0), false),
        ],
        others: Vec::new(),
        fast_path_slots: 16,
        max_locks_per_transaction: Some(64),
        server_version: 160_004,
        stats_reset: Some("2026-03-02 09:00:00+00".to_owned()),
        waits: None,
    };
    app.analysis.locks = vec![locks::footprint(&capture, &app.plan)];
    app.handle(Key::Char('L'), 10);
    insta::assert_snapshot!("locks", frame(&mut app, 100, 30));
    app.handle(Key::Char('j'), 10);
    assert_eq!(app.locks, Some(1));
    app.handle(Key::Esc, 10);
    assert!(app.locks.is_none());
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
    eprintln!("{per_frame:?} per frame");
    assert!(per_frame < limit(), "{per_frame:?} per frame");

    // The icicle lays out every node each frame.
    app.handle(Key::Char('F'), 10);
    explainsql_tui::render(&mut app, &theme, 120, 40);
    let start = Instant::now();
    for _ in 0..frames {
        app.handle(Key::Char('l'), 30);
        explainsql_tui::render(&mut app, &theme, 120, 40);
    }
    let per_frame = start.elapsed() / frames;
    eprintln!("{per_frame:?} per icicle frame");
    assert!(per_frame < limit(), "{per_frame:?} per icicle frame");
}

fn limit() -> Duration {
    // 16 ms in release builds; debug builds are several times slower.
    if cfg!(debug_assertions) {
        Duration::from_millis(200)
    } else {
        Duration::from_millis(16)
    }
}

/// The list of `explainsql top`: the costliest statements, one that
/// cannot be planned dimmed and explained; then, smaller, with --measure.
#[test]
fn statement_list() {
    use explainsql_core::top::{Entry, unplannable};
    let entry = |query: &str, calls: i64, total_ms: f64, read: i64, temp: i64| Entry {
        queryid: Some(calls * 7919),
        query: query.to_owned(),
        calls,
        total_ms,
        // Of a database whose statements took 1,000 s in all.
        share: total_ms / 1_000_000.0,
        mean_ms: total_ms / calls as f64,
        rows: calls * 3,
        shared_hit: read * 9,
        shared_read: read,
        temp_written: temp,
        unplannable: unplannable(query, Some(1024)),
    };
    let entries = vec![
        entry(
            "SELECT o.id, o.amount\n  FROM orders o\n WHERE o.customer_id = $1\n ORDER BY o.created_at DESC\n LIMIT $2",
            48_210,
            812_400.0,
            2_417,
            0,
        ),
        entry(
            "SELECT status, count(*) FROM orders GROUP BY status",
            1_204,
            96_300.0,
            24_170,
            3_412,
        ),
        entry("VACUUM (ANALYZE) orders", 12, 41_000.0, 30_000, 0),
        entry(
            "UPDATE orders SET note = $1 WHERE id = $2",
            310_000,
            12_900.0,
            12,
            0,
        ),
    ];
    let mut list = explainsql_tui::List::new(
        entries,
        "app@db.internal:5432/shop, PostgreSQL 16.4".to_owned(),
        true,
    );
    let theme = theme();
    insta::assert_snapshot!(
        "statement_list_120x40",
        text(&explainsql_tui::render_list(&mut list, &theme, 120, 40))
    );
    list.handle(Key::Char('j'));
    list.handle(Key::Char('j'));
    assert_eq!(list.handle(Key::Enter), None);
    list.measure = true;
    insta::assert_snapshot!(
        "statement_list_80x24",
        text(&explainsql_tui::render_list(&mut list, &theme, 80, 24))
    );
}
