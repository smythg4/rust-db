Output docs/demo.gif
Set FontSize 16
Set Width 1200
Set Height 700
Set TypingSpeed 40ms
Set WaitTimeout 15s

Hide
Type "rm -f /tmp/demo.db && cargo build --release -q" Enter
Type "clear" Enter
Wait /^>\s*$/
Show

Type "./target/release/repl /tmp/demo.db" Enter
Wait /rust-db >\s*$/
Type "insert 1 alice@example.com true" Enter
Wait /rust-db >\s*$/
Type "insert 2 null false" Enter
Wait /rust-db >\s*$/
Type "insert 3 bob@example.com true" Enter
Wait /rust-db >\s*$/
Type ".table" Enter
Wait /rust-db >\s*$/
Sleep 1.5s
Type ".quit" Enter
Wait /^>\s*$/
Sleep 500ms

Type "# restart: the data is still there" Enter
Wait /^>\s*$/
Type "./target/release/repl /tmp/demo.db" Enter
Wait /rust-db >\s*$/
Type "select * where active true" Enter
Wait /rust-db >\s*$/
Sleep 2s
Type ".quit" Enter
Wait /^>\s*$/
Sleep 1s