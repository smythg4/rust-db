use std::io::Write;

use rust_db::bpm::BufferPoolManager;
use rust_db::commontypes::TableId;
use rust_db::disk::FileDisk;
use rust_db::eviction::ClockEvictor;
use rust_db::schema::{Column, Schema};
use rust_db::table::Table;

fn main() {
    let mut stdin = std::io::stdin().lines();
    let mut stdout = std::io::stdout();

    let path = std::env::temp_dir().join(format!(
        "rust-db-{}-{}.db",
        std::process::id(),
        "table_basics"
    ));
    let _ = std::fs::remove_file(&path); // start clean

    let disk = FileDisk::new(&path).expect("failed to open file");
    let bpm = BufferPoolManager::new(disk, ClockEvictor::default(), 128);
    let schema = Schema::try_from(vec![
        Column::integer("id").unwrap(),
        Column::nullable_string("email").unwrap(),
        Column::bool("active").unwrap(),
    ])
    .expect("failed to build schema");
    let table =
        Table::create(&bpm, TableId::new(1), schema, "Users").expect("failed to create table");
    loop {
        print!("rust-db > ");
        stdout.flush().unwrap();
        let line = stdin.next().unwrap().unwrap();
        let parts: Vec<_> = line.split_whitespace().collect();
        match parts[0] {
            ".table" => table.print_table().unwrap(),
            ".quit" => break,
            "insert" => match table.insert_raw(&parts[1..]) {
                Ok(_) => println!("Insert successful"),
                Err(e) => eprintln!("Error on insert: {e:?}"),
            },
            "delete" => match table.delete_raw(&parts[1..]) {
                Ok(None) => println!("{:?} not found in table", &parts[1..]),
                Ok(Some(r)) => println!("Removed: {:?}", r),
                Err(e) => eprintln!("Error on delete: {e:?}"),
            },
            command => println!("Unknown command: '{command}'"),
        }
    }
}
