// Copyright (c) 2026 Elias Bachaalany
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::rc::Rc;

#[derive(Clone)]
struct Xref {
    from: i64,
    to: i64,
    kind: i32,
}

fn main() -> libxsql::Result<()> {
    let xrefs = Rc::new(vec![
        Xref {
            from: 0x1000,
            to: 0x2000,
            kind: 1,
        },
        Xref {
            from: 0x1004,
            to: 0x2000,
            kind: 1,
        },
        Xref {
            from: 0x2000,
            to: 0x3000,
            kind: 2,
        },
    ]);

    let def = libxsql::cached_table::<Xref>("xrefs")
        .estimate_rows({
            let xrefs = xrefs.clone();
            move || xrefs.len()
        })
        .cache_builder({
            let xrefs = xrefs.clone();
            move |cache| cache.extend(xrefs.iter().cloned())
        })
        .column_i64("from_ea", |row| row.from)
        .column_i64("to_ea", |row| row.to)
        .column_int("kind", |row| row.kind)
        .index_on("to_ea", |row| row.to)
        .build()?;

    let mut db = libxsql::Database::open_memory()?;
    db.register_and_create_cached_table(&def)?;

    let rows = db.query("SELECT printf('0x%X', from_ea), kind FROM xrefs WHERE to_ea = 0x2000")?;
    for row in rows.rows {
        println!("{}\t{}", &row[0], &row[1]);
    }
    Ok(())
}
