//! A small command line tool for inspecting a `lastro` database.
//!
//! There is no SQL yet, so this drives the storage layer directly. It exists
//! from the first stage because the usage experience matters as much as the
//! engine, and because being able to look inside a file is the cheapest
//! debugging tool there is.

use std::process::ExitCode;

use lastro::storage::page::{Page, PageType};
use lastro::storage::Pager;
use lastro::{PageId, Result, PAGE_SIZE};

const USAGE: &str = "\
lastro-cli — inspect a lastro database

usage:
    lastro-cli create <file>          create an empty database
    lastro-cli info <file>            print the metadata page
    lastro-cli pages <file>           summarize every page
    lastro-cli page <file> <number>   dump one page header
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let outcome = match refs.as_slice() {
        ["create", path] => create(path),
        ["info", path] => info(path),
        ["pages", path] => pages(path),
        ["page", path, number] => match number.parse::<PageId>() {
            Ok(id) => page(path, id),
            Err(_) => {
                eprintln!("not a page number: {number}");
                return ExitCode::FAILURE;
            }
        },
        _ => {
            eprint!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lastro: {error}");
            ExitCode::FAILURE
        }
    }
}

fn create(path: &str) -> Result<()> {
    Pager::create(path)?;
    println!("created {path}");
    Ok(())
}

fn info(path: &str) -> Result<()> {
    let mut pager = Pager::open(path)?;
    let meta = pager.meta().clone();

    println!("file                 {path}");
    println!("format version       {}", meta.format_version);
    println!("page size            {} bytes", meta.page_size);
    println!("pages                {}", meta.page_count);
    println!(
        "size                 {} bytes",
        meta.page_count as u64 * PAGE_SIZE as u64
    );
    println!("freelist head        {}", meta.freelist_head);
    println!("freelist length      {}", meta.freelist_count);
    println!("next txid            {}", meta.next_txid);
    println!("last checkpoint lsn  {}", meta.last_checkpoint_lsn);
    println!("catalog root         {}", meta.catalog_root);
    println!("schema version       {}", meta.schema_version);

    let freelist = pager.freelist()?;
    if !freelist.is_empty() {
        let shown: Vec<String> = freelist.iter().take(16).map(u32::to_string).collect();
        let ellipsis = if freelist.len() > 16 { ", ..." } else { "" };
        println!("freelist             {}{ellipsis}", shown.join(", "));
    }

    pager.check_invariants()?;
    println!("invariants           ok");
    Ok(())
}

fn pages(path: &str) -> Result<()> {
    let mut pager = Pager::open(path)?;
    let count = pager.page_count();
    let mut buffer = Page::zeroed();

    println!(
        "{:>6}  {:<9} {:>5} {:>5} {:>6} {:>10} {:>6}",
        "page", "type", "slots", "live", "free", "lsn", "extra"
    );
    for id in 0..count {
        pager.read_page(id, &mut buffer)?;
        let kind = describe(&buffer, id);
        let (slots, live, free) = if id == 0 {
            (0, 0, 0)
        } else {
            (
                buffer.slot_count(),
                buffer.live_count(),
                buffer.total_free(),
            )
        };
        println!(
            "{id:>6}  {kind:<9} {slots:>5} {live:>5} {free:>6} {:>10} {:>6}",
            buffer.lsn(),
            buffer.extra()
        );
    }
    Ok(())
}

fn page(path: &str, id: PageId) -> Result<()> {
    let mut pager = Pager::open(path)?;
    let mut buffer = Page::zeroed();
    pager.read_page(id, &mut buffer)?;

    println!("page                 {id}");
    println!("type                 {}", describe(&buffer, id));
    if id == 0 {
        println!("(the metadata page; use `info` to read it)");
        return Ok(());
    }

    println!("root                 {}", buffer.is_root());
    println!("slots                {}", buffer.slot_count());
    println!("live slots           {}", buffer.live_count());
    println!("free contiguous      {} bytes", buffer.free_space());
    println!("free total           {} bytes", buffer.total_free());
    println!("fragmented           {} bytes", buffer.fragmented());
    println!("lsn                  {}", buffer.lsn());
    println!("extra                {}", buffer.extra());

    match buffer.check_invariants() {
        Ok(()) => println!("invariants           ok"),
        Err(error) => println!("invariants           VIOLATED: {error}"),
    }

    for (slot, bytes) in buffer.iter_cells().take(20) {
        println!(
            "  slot {slot:<4} {:>5} bytes  {}",
            bytes.len(),
            preview(bytes)
        );
    }
    Ok(())
}

fn describe(page: &Page, id: PageId) -> &'static str {
    if id == 0 {
        return "meta";
    }
    match page.page_type() {
        Some(PageType::Meta) => "meta",
        Some(PageType::Interior) => "interior",
        Some(PageType::Leaf) => "leaf",
        Some(PageType::Heap) => "heap",
        Some(PageType::Freelist) => "free",
        Some(PageType::Overflow) => "overflow",
        None => "unknown",
    }
}

/// A short, printable rendering of a cell's first bytes.
fn preview(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &byte in bytes.iter().take(24) {
        if byte.is_ascii_graphic() || byte == b' ' {
            out.push(byte as char);
        } else {
            out.push('.');
        }
    }
    if bytes.len() > 24 {
        out.push_str("...");
    }
    out
}
