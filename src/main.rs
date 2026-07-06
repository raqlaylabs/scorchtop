use clap::{Parser, Subcommand};
use serde_json::json;

use agentop::aggregate::{aggregate, Aggregates, Totals};
use agentop::source::claude_code::ClaudeCodeSource;
use agentop::source::{ScanStats, Source};

#[derive(Parser)]
#[command(name = "agentop", version, about = "btop for AI coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print aggregate usage totals.
    Dump {
        /// Emit JSON (machine-readable, used by the verify oracle).
        #[arg(long)]
        json: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Dump { json }) => dump(json),
        None => {
            // Milestone 2 will launch the dashboard here.
            eprintln!("dashboard not implemented yet — try `agentop dump --json`");
            std::process::exit(2);
        }
    }
}

fn dump(as_json: bool) {
    let Some(source) = ClaudeCodeSource::new() else {
        eprintln!("could not locate home directory");
        std::process::exit(1);
    };
    let scan = source.scan();
    let agg = aggregate(&scan.records);

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&to_json(source.name(), &agg, &scan.stats)).unwrap()
        );
    } else {
        print_summary(&agg, &scan.stats);
    }
}

fn totals_json(t: &Totals) -> serde_json::Value {
    json!({
        "input_tokens": t.tokens.input,
        "output_tokens": t.tokens.output,
        "cache_creation_tokens": t.tokens.cache_create,
        "cache_read_tokens": t.tokens.cache_read,
        "total_tokens": t.tokens.total(),
        "records": t.records,
        "est_cost_usd": if t.has_unknown_model && t.known_cost == 0.0 {
            serde_json::Value::Null
        } else {
            json!((t.known_cost * 100.0).round() / 100.0)
        },
        "cost_incomplete": t.has_unknown_model,
    })
}

fn to_json(source: &str, agg: &Aggregates, stats: &ScanStats) -> serde_json::Value {
    let mut projects: Vec<_> = agg.by_project.iter().collect();
    projects.sort_by_key(|(_, p)| std::cmp::Reverse(p.totals.tokens.total()));

    json!({
        "source": source,
        "totals": totals_json(&agg.totals),
        "days": agg.by_day.iter().map(|(day, t)| {
            let mut v = totals_json(t);
            v["date"] = json!(day.to_string());
            v
        }).collect::<Vec<_>>(),
        "projects": projects.iter().map(|(key, p)| {
            let mut v = totals_json(&p.totals);
            v["key"] = json!(key);
            v["name"] = json!(p.display_name);
            v
        }).collect::<Vec<_>>(),
        "models": agg.by_model.iter().map(|(model, t)| {
            let mut v = totals_json(t);
            v["model"] = json!(model);
            v
        }).collect::<Vec<_>>(),
        "stats": {
            "files_scanned": stats.files_scanned,
            "malformed_lines": stats.malformed_lines,
            "duplicates_skipped": agg.duplicates_skipped,
        },
    })
}

fn print_summary(agg: &Aggregates, stats: &ScanStats) {
    let t = &agg.totals;
    println!("agentop — {} records across {} files", t.records, stats.files_scanned);
    println!(
        "tokens: {} in / {} out / {} cache-write / {} cache-read",
        t.tokens.input, t.tokens.output, t.tokens.cache_create, t.tokens.cache_read
    );
    let suffix = if t.has_unknown_model { " (some models unpriced)" } else { "" };
    println!("est. API value: ${:.2}{}", t.known_cost, suffix);
    println!(
        "projects: {} | days: {} | models: {} | duplicates skipped: {} | malformed: {}",
        agg.by_project.len(),
        agg.by_day.len(),
        agg.by_model.len(),
        agg.duplicates_skipped,
        stats.malformed_lines
    );
}
