# SQL Server TUI

Simple terminal query editor for SQL Server, autocomplete, and export to CSV or XLSX. This is possible thanks to [tiberius](https://github.com/tiberius-rs/tiberius/tree/main).

```
 sst  sa@db.internal:1433  ShopDB  42 tables
╭ Query ──────────────────────────────────────────────────────────────────────╮
│SELECT * FROM Sales.Orders o                                                 │
│WHERE o.tot                                                                  │
│         ╭────────────────────────────────────────────╮                      │
╰─────────│TotalAmount  decimal                        │─ ^E $EDITOR ─────────╯
╭ Results │TotalTax     decimal                        │ · OrderID int ───────╮
│OrderID  ╰────────────────────────────────────────────╯  Status     Notes    │
│1902      403          2026-03-13 18:00:00   19000.98    open       note 1902│
│1903      404          2026-03-13 17:00:00   19010.97    shipped    NULL     │
╰─────────────────────────────────────────────────────────────────────────────╯
 99 rows in 12.4ms  ^Enter run · Tab complete · e export · ^B queries · ? help
```

## Install

```sh
cargo install --git https://github.com/sanisoclem/sst
```

## Limitations

I have only tested this in Linux, YMMV.

The grid only shows `--max-rows` rows.

