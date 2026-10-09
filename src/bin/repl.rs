use clap::Parser;
use rust_db::bpm::{BpmError, BufferPoolManager, ClockEvictor};
use rust_db::disk::FileDisk;
use rust_db::schema::{Column, Schema};
use rust_db::table::{Table, TableError};
use rust_db::types::TableId;
use std::io::Write;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// filepath to the requested database
    filepath: PathBuf,

    /// Optional pool_size
    #[arg(long, default_value_t = 64)]
    pool_size: usize,
}

const COMMANDS: &[(&str, &str)] = &[
    (".help", "show this message"),
    (".table", "print the table (first and last 5 rows)"),
    (".quit", "save and exit"),
    (".size", "prints size of db file"),
    (".vacuum", "de-fragments the on disk storage"),
    ("insert", "insert <id> <email|null> <true|false>"),
    ("delete", "delete <id>"),
    ("select", "select <id> (WHERE <col> <val>)"),
];

fn main() {
    env_logger::init();
    let cli = Cli::parse();
    let mut stdin = std::io::stdin().lines();
    let mut stdout = std::io::stdout();

    let path = cli.filepath;
    let pool_size = cli.pool_size;

    let disk = FileDisk::new(&path).expect("failed to open file");
    let bpm = BufferPoolManager::new(disk, ClockEvictor::default(), pool_size);
    let schema = Schema::try_from(vec![
        Column::integer("id").unwrap(),
        Column::nullable_string("email").unwrap(),
        Column::bool("active").unwrap(),
    ])
    .expect("failed to build schema");
    let table = match Table::open(&bpm, TableId::new(1)) {
        Ok(t) => t,
        Err(TableError::Bpm(BpmError::Io(e))) if e.kind() == std::io::ErrorKind::NotFound => {
            Table::create(&bpm, TableId::new(1), schema, "Users").expect("failed to create table")
        }
        Err(e) => panic!("Unexpected TableError: {e:?}"),
    };

    loop {
        print!("rust-db > ");
        stdout.flush().unwrap();
        let line = stdin.next().unwrap().unwrap();
        let parts: Vec<_> = line
            .split_whitespace()
            .map(|p| p.to_ascii_lowercase())
            .collect();
        match parts[0].as_str() {
            ".table" => table.print_table().unwrap(),
            ".quit" => {
                table.close().expect("failed to close the table");
                bpm.close().expect("failed to close the bpm");
                break;
            }
            ".size" => {
                match table.size() {
                    Ok(n) => println!("File size: {n} bytes"),
                    Err(e) => eprintln!("Error on size: {e}"),
                };
            }
            ".vacuum" => match table.vacuum() {
                Ok(_) => println!("Vacuum successful!"),
                Err(e) => eprintln!("Error on vacuum: {e}"),
            },
            ".help" => print_commands(),
            "insert" => match table.insert_raw(&parts[1..]) {
                Ok(_) => println!("Insert successful"),
                Err(e) => eprintln!("Error on insert: {e}"),
            },
            "delete" => match table.delete_raw(&parts[1..]) {
                Ok(None) => println!("{:?} not found in table", &parts[1..]),
                Ok(Some(r)) => println!("Removed: {r:?}"),
                Err(e) => eprintln!("Error on delete: {e}"),
            },
            "select" if parts.len() == 2 => match table.get_raw(&parts[1..]) {
                Ok(None) => println!("{:?} not found in table", &parts[1..]),
                Ok(Some(r)) => println!("Found: {r:?}"),
                Err(e) => eprintln!("Error on get: {e}"),
            },
            "select" if parts.len() == 5 => match table.get_all_raw(&parts[1..]) {
                Ok(v) => {
                    println!("Found:");
                    table.print_rows(&v).unwrap();
                }
                Err(e) => eprintln!("Error on get: {e}"),
            },
            command => println!("Unknown command: '{command}'"),
        }
    }
}

fn print_commands() {
    for (command, description) in COMMANDS {
        println!("   {command:<8}: {description}");
    }
}
