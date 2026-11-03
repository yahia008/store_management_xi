# Requirements traceability (FR-01 to FR-19)

Each requirement, the endpoints that implement it, and the automated tests that prove it. Run every test with
`DATABASE_URL=postgres://store:store@127.0.0.1:5433/store cargo test` (start the database with `./scripts/dev-db.sh`).
All endpoints are under `/api/v1`. The wording of FR-07 to FR-09 was reconstructed from the project scope, so check it
against the SRS.

| FR | What it covers | Endpoints | Tests (file :: name) |
|----|----------------|-----------|----------------------|
| 01 | Business profile, first owner | `POST /setup`, `GET/PATCH /business` | m1_business_and_stores :: fr01_setup_runs_once_and_profile_is_editable |
| 02 | Stores (Lagos, Abuja, Kaduna …) | `GET/POST /stores`, `GET/PATCH /stores/{id}` | m1_business_and_stores :: fr02_stores_are_created_listed_and_deactivated |
| 03 | Business and store settings, tax | `GET/PUT /business/settings`, `GET/POST /business/tax-rates`, `GET/PUT /stores/{id}/settings` | m1_business_and_stores :: fr03_cashier_cannot_change_tax_or_currency, fr03_tax_rates_are_effective_dated_and_never_rewritten, fr03_currency_and_timezone_are_validated, fr03_store_settings_belong_to_the_store_manager |
| 04 | Accounts, sign-in, sessions, passwords | `POST /auth/login`, `/auth/logout`, `/auth/password/{forgot,reset,change}`, `GET /me`, `POST /employees/{id}/sessions/revoke` | m1_access :: fr04_* (10 tests: login/logout, hashing, lockout, no user enumeration, reset single-use and expiry, password change, idle timeout, deactivation) |
| 05 | Roles, one person in several stores | `GET /roles`, `GET/POST /employees`, `GET/PATCH /employees/{id}`, `POST /employees/{id}/roles`, `DELETE /employees/{id}/roles/{membership}` | m1_access :: fr05_one_person_can_hold_different_roles_in_different_stores, fr05_role_and_store_must_agree, fr05_only_the_owner_hands_out_business_wide_roles, fr05_the_last_owner_cannot_be_removed |
| 06 | Store-level access control, audit | every store route (permission from the database, never from a client-sent store id); `GET /audit` | m1_access :: fr06_a_lagos_cashier_cannot_reach_abuja_by_changing_the_store_id, fr06_staff_lists_are_scoped_to_the_managers_stores, fr06_the_audit_trail_cannot_be_edited_or_deleted |
| 07 | Categories and products | `GET/POST /categories`, `PATCH /categories/{id}`, `GET/POST /products`, `GET/PATCH /products/{id}` | m2_catalog :: fr07_categories_form_a_tree_without_loops, fr07_products_are_created_found_and_archived_not_deleted, fr07_product_input_is_validated, fr07_staff_can_read_the_catalogue_but_not_change_it_or_see_cost |
| 08 | Variants, SKU, barcode | `POST /products/{id}/variants`, `PATCH /variants/{id}` | m2_catalog :: fr08_variants_have_unique_sku_and_barcode |
| 09 | Price history, store prices | `GET/POST /variants/{id}/prices` | m2_catalog :: fr09_prices_are_a_history_and_future_prices_wait, fr09_a_store_can_have_its_own_price |
| 10 | Stock per store | `GET /stores/{s}/stock`, `GET /stores/{s}/stock/{variant}`, `POST /stores/{s}/stock/receipts` | m3_inventory :: fr10_each_store_holds_its_own_stock |
| 11 | One movement service and ledger | all stock routes call `inventory::apply` / `consume` | m3_inventory :: fr11_every_change_is_a_ledger_row_and_the_ledger_adds_up, fr11_stock_cannot_go_negative_even_when_requests_race |
| 12 | Adjustments, approval, counts | `POST/GET /stores/{s}/stock/adjustments`, `…/{id}/approve`, `…/{id}/reject`, `POST …/counts`, `GET/PUT …/settings` | m3_inventory :: fr12_big_adjustments_wait_for_someone_else_to_approve, fr12_the_approval_threshold_is_set_per_store_by_the_manager, fr12_a_count_fixes_small_differences_and_queues_large_ones |
| 13 | Batches, expiry (FEFO), damaged | `POST …/receipts` (batch + expiry), `GET …/expiring`, `POST …/expire`, adjustment kind `damage` | m3_inventory :: fr13_batches_are_used_earliest_expiry_first, fr13_fefo_skips_expired_batches_and_the_sweep_separates_them, fr13_damaged_goods_are_kept_apart_from_sellable_stock |
| 14 | Low-stock alerts | `PUT …/reorder-levels/{variant}`, `GET …/alerts`, `GET …/stock?low=true` | m3_inventory :: fr14_low_stock_raises_one_alert_and_clears_when_restocked |
| 15 | Sales | `POST /stores/{s}/sales` (needs `Idempotency-Key`), `GET /stores/{s}/sales`, `GET /sales/{id}` | m4_sales :: fr15_* (9 tests: totals, tax rounding, server-set prices, store price, history kept, rollback on short stock, retries, last-unit race, visibility) |
| 16 | Payments (cash/card/transfer, split, change) | part of `POST /stores/{s}/sales` | m4_sales :: fr16_payments_must_cover_the_total_and_only_cash_gives_change |
| 17 | Receipts and numbering | `GET /sales/{id}/receipt`, numbering from `PUT /stores/{s}/settings` | m4_sales :: fr17_the_receipt_has_everything_needed_to_print, fr15_not_enough_stock_stops_the_whole_sale_and_gives_the_number_back |
| 18 | Returns and refunds | `POST /sales/{id}/returns` (needs `Idempotency-Key`) | m4_sales :: fr18_returns_refund_exactly_and_put_stock_back, fr18_a_retried_return_refunds_once, fr18_late_returns_and_big_refunds_need_a_manager, fr18_returned_perishables_go_back_to_the_batch_they_came_from |
| 19 | Customers | `GET/POST /customers`, `GET/PATCH /customers/{id}`, `GET /customers/{id}/sales` | m4_sales :: fr19_customers_are_recorded_found_and_linked_to_sales |

Unit tests for the money rules (tax rounding, exact partial refunds) are in `src/money.rs`.

## Design rules that apply across requirements

| Rule | Where it is enforced |
|------|----------------------|
| Money is whole minor units, never floating point (NFR-30) | `i64` columns and `src/money.rs`; tax is rounded half up per line |
| Permissions come from the signed-in user's memberships, not from a client-supplied store id | `Principal` in `src/auth.rs`, `authorize_store` |
| Passwords hashed with Argon2id; session and reset tokens stored only as SHA-256 hashes (NFR-16) | `src/security.rs`; test fr04_passwords_and_tokens_are_not_stored_in_the_clear |
| Audit, stock-movement, price-history and sales records cannot be edited or deleted | database triggers in `migrations/`; tests fr06_the_audit_trail_cannot_be_edited_or_deleted, fr11_…, fr09_…, fr15_sales_are_visible_… |
| Stock never goes negative, even under concurrent requests | conditional `UPDATE … WHERE balance + delta >= 0`; tests fr11_…race, fr15_two_tills_… |
| A retried sale or return happens once | `Idempotency-Key` plus an advisory lock; tests fr15_a_retried_sale_…, fr18_a_retried_return_… |

## Known limits (not built, by choice)

- Passwords-reset and low-stock alerts are stored/logged only; nothing is sent by email or SMS (`ResetNotifier` is the plug-in point).
- The expiry sweep runs when called (`POST …/stock/expire`); schedule it externally.
- No discounts, loyalty points, store credit, voiding of a whole sale, or transfers between stores.
- Sessions are bearer tokens; there is no rate limiting per IP (lockout is per account).
