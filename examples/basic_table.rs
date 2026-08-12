// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use std::rc::Rc;

struct Product {
    id: i32,
    name: String,
    price: f64,
}

fn main() -> libxsql::Result<()> {
    let products = Rc::new(vec![
        Product {
            id: 1,
            name: "Apple".to_string(),
            price: 1.50,
        },
        Product {
            id: 2,
            name: "Banana".to_string(),
            price: 0.75,
        },
        Product {
            id: 3,
            name: "Cherry".to_string(),
            price: 3.00,
        },
    ]);

    let def = libxsql::table("products")
        .count({
            let products = products.clone();
            move || products.len()
        })
        .column_int("id", {
            let products = products.clone();
            move |i| products[i].id
        })
        .column_text("name", {
            let products = products.clone();
            move |i| products[i].name.clone()
        })
        .column_double("price", {
            let products = products.clone();
            move |i| products[i].price
        })
        .build()?;

    let mut db = libxsql::Database::open_memory()?;
    db.register_and_create_table(&def)?;

    let rows = db.query("SELECT id, name, price FROM products ORDER BY id")?;
    for row in rows.rows {
        println!("{}\t{}\t{}", &row[0], &row[1], &row[2]);
    }
    Ok(())
}
