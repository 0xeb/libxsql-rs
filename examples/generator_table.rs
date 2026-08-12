// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use std::rc::Rc;

#[derive(Clone)]
struct Decompiled {
    func_ea: i64,
    pseudocode: String,
}

struct DecompiledGenerator {
    rows: Rc<Vec<Decompiled>>,
    pos: Option<usize>,
}

impl DecompiledGenerator {
    fn new(rows: Rc<Vec<Decompiled>>) -> Self {
        Self { rows, pos: None }
    }
}

impl libxsql::Generator<Decompiled> for DecompiledGenerator {
    fn next(&mut self) -> libxsql::Result<bool> {
        let next = self.pos.map_or(0, |pos| pos + 1);
        if next < self.rows.len() {
            self.pos = Some(next);
            Ok(true)
        } else {
            self.pos = None;
            Ok(false)
        }
    }

    fn current(&self) -> &Decompiled {
        &self.rows[self.pos.expect("generator current row is valid")]
    }

    fn rowid(&self) -> i64 {
        self.current().func_ea
    }
}

fn main() -> libxsql::Result<()> {
    let rows = Rc::new(vec![
        Decompiled {
            func_ea: 0x1000,
            pseudocode: "int start() { return 0; }".to_string(),
        },
        Decompiled {
            func_ea: 0x2000,
            pseudocode: "int helper() { return 1; }".to_string(),
        },
    ]);

    let def = libxsql::generator_table::<Decompiled>("decompiled")
        .estimate_rows({
            let rows = rows.clone();
            move || rows.len()
        })
        .generator({
            let rows = rows.clone();
            move || Box::new(DecompiledGenerator::new(rows.clone()))
        })
        .column_i64("func_ea", |row| row.func_ea)
        .column_text("pseudocode", |row| row.pseudocode.clone())
        .build()?;

    let mut db = libxsql::Database::open_memory()?;
    db.register_and_create_generator_table(&def)?;

    let rows = db.query("SELECT printf('0x%X', func_ea), pseudocode FROM decompiled LIMIT 1")?;
    for row in rows.rows {
        println!("{}\t{}", &row[0], &row[1]);
    }
    Ok(())
}
