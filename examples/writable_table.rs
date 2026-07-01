// Copyright (c) 2026 Elias Bachaalany
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::cell::RefCell;
use std::rc::Rc;

struct Task {
    id: i32,
    title: String,
    done: bool,
}

fn main() -> libxsql::Result<()> {
    let tasks = Rc::new(RefCell::new(vec![
        Task {
            id: 1,
            title: "Write documentation".to_string(),
            done: false,
        },
        Task {
            id: 2,
            title: "Fix bug".to_string(),
            done: false,
        },
    ]));

    let def = libxsql::table("tasks")
        .count({
            let tasks = tasks.clone();
            move || tasks.borrow().len()
        })
        .column_int("id", {
            let tasks = tasks.clone();
            move |i| tasks.borrow()[i].id
        })
        .column_text_rw(
            "title",
            {
                let tasks = tasks.clone();
                move |i| tasks.borrow()[i].title.clone()
            },
            {
                let tasks = tasks.clone();
                move |i, value| {
                    tasks.borrow_mut()[i].title = value.to_string();
                    true
                }
            },
        )
        .column_int_rw(
            "done",
            {
                let tasks = tasks.clone();
                move |i| i32::from(tasks.borrow()[i].done)
            },
            {
                let tasks = tasks.clone();
                move |i, value| {
                    tasks.borrow_mut()[i].done = value != 0;
                    true
                }
            },
        )
        .deletable({
            let tasks = tasks.clone();
            move |i| {
                tasks.borrow_mut().remove(i);
                true
            }
        })
        .build()?;

    let mut db = libxsql::Database::open_memory()?;
    db.register_and_create_table(&def)?;
    db.exec("UPDATE tasks SET done = 1 WHERE id = 2")?;
    db.exec("DELETE FROM tasks WHERE done = 1")?;

    let rows = db.query("SELECT id, title, done FROM tasks ORDER BY id")?;
    for row in rows.rows {
        println!("{}\t{}\t{}", &row[0], &row[1], &row[2]);
    }
    Ok(())
}
