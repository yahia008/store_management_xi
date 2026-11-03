# Requirements traceability: sections 1 to 5 (FR-01 to FR-19)

Written against *Store Management System SRS v1.0* (20 October 2026). For each requirement: what implements it,
the test that proves it, and any exception or limitation, as Part C of the SRS asks.

Run every test with `DATABASE_URL=postgres://store:store@127.0.0.1:5433/store cargo test`
(start the database with `./scripts/dev-db.sh`). Last full run: **69 passed, 0 failed**.
Endpoints are under `/api/v1`. Test names are `file :: test`.

## 1. Business and Store Management

| FR | Requirement | Implemented by | Tests | Exceptions / limits |
|----|-------------|----------------|-------|---------------------|
| 01 | Business Registration and Profile Management | `POST /setup`, `GET/PATCH /business`; every change stores `updated_by` and time and an audit entry with before/after | m1_business_and_stores :: fr01_setup_runs_once_and_profile_is_editable | The logo is a URL; there is no file upload. |
| 02 | Store Creation and Management | `GET/POST /stores` (`?q=` name or id), `GET/PATCH /stores/{id}`; a deactivated store refuses sales, receipts, adjustments, counts, expiry sweeps, returns (owners/administrators may still do administrative work) | m1_business_and_stores :: fr02_stores_are_created_listed_and_deactivated; m5_srs_gaps :: fr02_a_deactivated_store_takes_no_new_sales_or_stock_movements, fr02_stores_can_be_searched_by_name_or_identifier | Purchases and transfers (section 6) do not exist yet, so cannot be blocked. |
| 03 | Business and Store Settings | `GET/PUT /business/settings`, `GET/POST /business/tax-rates`, `GET/PUT /stores/{id}/settings` | m1_business_and_stores :: fr03_cashier_cannot_change_tax_or_currency, fr03_tax_rates_are_effective_dated_and_never_rewritten, fr03_currency_and_timezone_are_validated, fr03_store_settings_belong_to_the_store_manager | Effective-date policy: a new tax rate or price starts now or later, never in the past; sales keep what they were made with (m4_sales :: fr15_old_sales_keep_the_price_and_tax_they_were_made_with). |

## 2. Accounts and Access Control

| FR | Requirement | Implemented by | Tests | Exceptions / limits |
|----|-------------|----------------|-------|---------------------|
| 04 | Authentication and Session Management | `POST /auth/login`, `/auth/logout`, `/auth/password/{forgot,reset,change}`, `GET /me`, `POST /employees/{id}/sessions/revoke` | m1_access :: fr04_* (login/logout, no plaintext, lockout after 5, no user enumeration, single-use and expiring reset, password change, idle expiry, deactivation) | Recovery works through a reset token handed to a `ResetNotifier`; no email or SMS transport is connected (it logs). |
| 05 | Employee and Role Management | `GET /roles`, `GET/POST /employees`, `GET/PATCH /employees/{id}`, `POST /employees/{id}/roles`, `DELETE /employees/{id}/roles/{m}` | m1_access :: fr05_* (manager in Lagos and cashier in Abuja, role/store agreement, owner-only business-wide roles, last owner protected) | Each role has fixed permissions; there is no screen to edit permissions per person. |
| 06 | Business and Store-Level Authorization | every protected request builds the user's rights from the database; a client-sent store id is only checked against them; refusals are logged | m1_access :: fr06_a_lagos_cashier_cannot_reach_abuja_by_changing_the_store_id, fr06_staff_lists_are_scoped_to_the_managers_stores, fr06_the_audit_trail_cannot_be_edited_or_deleted | Only one business can be set up through `/setup`; cross-business isolation is enforced in every query but tested with one business. |

## 3. Product Management

| FR | Requirement | Implemented by | Tests | Exceptions / limits |
|----|-------------|----------------|-------|---------------------|
| 07 | Product Catalogue Management | `GET/POST /products`, `GET/PATCH /products/{id}` (name, SKU, category, description, unit, brand, image URL, extra attributes, cost, selling price, active/archived); archived, never deleted | m2_catalog :: fr07_products_are_created_found_and_archived_not_deleted, fr07_product_input_is_validated, fr07_staff_can_read_the_catalogue_but_not_change_it_or_see_cost; m5_srs_gaps :: fr07_products_carry_brand_image_and_extra_attributes | SKU, cost and selling price live on the variant (every product has at least one). Images are URLs. |
| 08 | Categories, Variants, and Product Identification | `GET/POST /categories`, `PATCH /categories/{id}`, `POST /products/{id}/variants`, `PATCH /variants/{id}`, barcode lookup `?barcode=`, SKU/name search | m2_catalog :: fr07_categories_form_a_tree_without_loops, fr08_variants_have_unique_sku_and_barcode; m4_sales :: fr15_discounts_barcodes_and_the_quote_before_payment (sell by barcode or SKU) | |
| 09 | Pricing and Price History | `GET/POST /variants/{id}/prices`; every row records the new price, the previous price, effective date, user and time; a store can have its own price | m2_catalog :: fr09_prices_are_a_history_and_future_prices_wait, fr09_a_store_can_have_its_own_price; m5_srs_gaps :: fr09_each_price_records_the_price_it_replaced; m4_sales :: fr15_old_sales_keep_the_price_and_tax_they_were_made_with | Cost-price changes are kept as before/after in the audit log, not in a separate table. Price-change approval thresholds belong to FR-29 (section 7), not built. |

## 4. Inventory and Stock Control

| FR | Requirement | Implemented by | Tests | Exceptions / limits |
|----|-------------|----------------|-------|---------------------|
| 10 | Store-Level Inventory Tracking | `GET /stores/{s}/stock` (filter `q`, `category_id`, `status`; shows `on_hand`, `available`, damaged, expired), `GET /stores/{s}/stock/{variant}`, `POST .../receipts` | m3_inventory :: fr10_each_store_holds_its_own_stock; m5_srs_gaps :: fr10_stock_can_be_filtered_and_shows_what_is_really_available | Reservations are not enabled, so `reserved` is always 0 (the SRS says "if reservations are enabled"). |
| 11 | Centralized Stock Movement Processing | every stock change goes through `inventory::apply`/`consume`; the balance and the ledger row are written in one transaction; `GET .../stock/movements` | m3_inventory :: fr11_every_change_is_a_ledger_row_and_the_ledger_adds_up, fr11_stock_cannot_go_negative_even_when_requests_race; m4_sales :: fr15_not_enough_stock_stops_the_whole_sale_and_gives_the_number_back | Purchase and transfer movements arrive with section 6. |
| 12 | Stock Adjustments and Physical Counts | `POST/GET .../stock/adjustments` (reason required, optional reference), `.../approve`, `.../reject`, `POST .../counts`, `GET/PUT .../stock/settings` (approval threshold) | m3_inventory :: fr12_big_adjustments_wait_for_someone_else_to_approve, fr12_the_approval_threshold_is_set_per_store_by_the_manager, fr12_a_count_fixes_small_differences_and_queues_large_ones; m5_srs_gaps :: fr12_fr14_adjustments_keep_a_reference_and_the_expiry_warning_is_a_setting | Negative stock is never allowed (no policy switch to permit it). A count under the threshold is applied immediately and recorded as an auto-approved adjustment. |
| 13 | Low-Stock Monitoring | `PUT .../reorder-levels/{variant}`, `GET .../stock?status=low` (with `q`/`category_id`), `GET .../stock/alerts`; one open alert per store and variant | m3_inventory :: fr14_low_stock_raises_one_alert_and_clears_when_restocked | The alert is stored and listed through the API; the in-app notification centre (read/unread, delivery) is FR-26 and not built. |
| 14 | Inventory Batches, Expiry, and Damaged Goods | receipts with batch code and expiry, FEFO selling, `GET .../stock/expiring` (configurable warning days), `POST .../stock/expire`, adjustment kind `damage`; damaged and expired are separate buckets | m3_inventory :: fr13_batches_are_used_earliest_expiry_first, fr13_fefo_skips_expired_batches_and_the_sweep_separates_them, fr13_damaged_goods_are_kept_apart_from_sellable_stock; m5_srs_gaps :: fr12_fr14_adjustments_keep_a_reference_and_the_expiry_warning_is_a_setting | Expired stock is never sold; there is no "authorized policy" switch to allow it. The expiry sweep runs when called; schedule it externally. (Test names `fr13_*` predate the SRS numbering and cover FR-14.) |

## 5. Sales, Returns, and Customers

| FR | Requirement | Implemented by | Tests | Exceptions / limits |
|----|-------------|----------------|-------|---------------------|
| 15 | Point-of-Sale Transactions | `POST /stores/{s}/sales/quote` (review the total), `POST /stores/{s}/sales` (needs `Idempotency-Key`); lines by variant, barcode or SKU; line discounts (cashier limit 10%, more needs a manager); tax per line, half-up | m4_sales :: fr15_* (totals and numbering, rounding, server-set prices, store price, history kept, rollback on short stock, retries, last-unit race, visibility); m5_srs_gaps :: fr15_discounts_barcodes_and_the_quote_before_payment | The basket (add/remove items, change quantities) lives in the client; the server prices it on request. Line discounts only: no whole-basket or percentage promotions. |
| 16 | Payment Recording and Reconciliation | payments in the sale (`cash`, `card`, `transfer`, `other`; split; change from cash only), statuses pending/successful/failed/reversed/refunded, `POST /sales/{id}/payments` and `.../payments/{pid}/{confirm,fail,reverse}`, `GET /stores/{s}/reports/payment-reconciliation` | m4_sales :: fr16_payments_must_cover_the_total_and_only_cash_gives_change; m5_srs_gaps :: fr16_a_sale_is_paid_only_when_successful_payments_cover_it, fr16_a_repeated_payment_reference_is_not_counted_twice, fr16_reconciliation_ties_payments_back_to_sales | Credit and pay-later sales are not enabled (the SRS allows this: "subject to explicitly configured policies"). No payment-provider integration; card and transfer details are recorded by reference only, and no card numbers are stored. |
| 17 | Receipts, Invoices, and Sales History | `GET /sales/{id}/receipt`, `POST /sales/{id}/receipt/print` (first print is `ORIGINAL`, later ones `REPRINT #n`, manager/finance only), `GET /stores/{s}/sales` (filter by date, cashier, transaction, payment status), `GET /sales/{id}` | m4_sales :: fr17_the_receipt_has_everything_needed_to_print, fr15_sales_are_visible_only_to_the_people_who_should_see_them; m5_srs_gaps :: fr17_sales_can_be_searched_and_reprints_are_marked | Returns structured receipt data; the printed layout is the client's job. There is no separate invoice document; the receipt serves as one. |
| 18 | Sales Returns and Refunds | `POST /sales/{id}/returns` (needs `Idempotency-Key`): partial or full, condition (`restock` or `damaged`), exact proportional refunds, return period and refund limit, non-returnable goods and approvals need a manager and the approver is recorded, refund recorded separately with status | m4_sales :: fr18_returns_refund_exactly_and_put_stock_back, fr18_a_retried_return_refunds_once, fr18_late_returns_and_big_refunds_need_a_manager, fr18_returned_perishables_go_back_to_the_batch_they_came_from; m5_srs_gaps :: fr18_refunds_carry_a_status_and_their_approver | Return period and refund limit come from environment settings for the whole business, not per store. A refund is recorded as done (`refunded`); there is no pending-refund workflow. |
| 19 | Customer Records | `GET/POST /customers`, `GET/PATCH /customers/{id}`, `GET /customers/{id}/sales`; walk-in sales need no customer; access limited to people who serve customers | m4_sales :: fr19_customers_are_recorded_found_and_linked_to_sales; m5_srs_gaps :: fr15_discounts_barcodes_and_the_quote_before_payment (walk-in sale) | Privacy workflows (NDPA/GDPR requests, NFR-29) are not built. |

## Design constraints from Part B that the above relies on

| NFR | Status | Evidence |
|-----|--------|----------|
| 09 Inventory consistency and concurrency | Done | fr11_stock_cannot_go_negative_even_when_requests_race, fr15_two_tills_cannot_sell_the_last_unit_twice |
| 10 Idempotency | Done | fr15_a_retried_sale_is_not_sold_twice, fr18_a_retried_return_refunds_once, fr16_a_repeated_payment_reference_is_not_counted_twice (10 replays) |
| 16 Password and authentication security | Done | Argon2id; lockout after 5; single-use reset tokens (m1_access :: fr04_*) |
| 17 Staff authentication and session security | Partly | Admin idle timeout 15 minutes and revocation on deactivation/reset are done and tested. **MFA is not built.** |
| 30 Currency and financial accuracy | Done for one currency | Integer minor units; documented half-up rounding (`src/money.rs`, unit tests); history never rewritten. Multi-currency is not enabled. |
| 03 Sales processing | Done (correctness) | A failed sale leaves nothing behind (fr15_not_enough_stock_...). No load test has been run, so the timing targets are unverified. |
| 20 API abuse protection | **Not built** | Lockout is per account only; there is no per-client rate limiting. |
| 26 Testing and API documentation | Partly | Integration tests cover success, failure, duplicates and concurrency on the critical paths. Line coverage has not been measured and there is no API reference beyond this file and the code. |
| 01-02, 04-08, 11-15, 18-19, 21-25, 27-29 | Not addressed by this code | These cover load testing, capacity, availability, backup and recovery, encryption in transit and at rest, payment-card compliance, security scanning, the user interface, browser support, deployment, monitoring and privacy. They are deployment, interface or process work, and none of it has been done or verified here. |

## Rules that hold across requirements

| Rule | Where it is enforced |
|------|----------------------|
| Money is whole minor units, never floating point | `i64` columns and `src/money.rs` |
| Rights come from the signed-in user's memberships, not from a client-sent store id | `Principal` in `src/auth.rs`, `authorize_store` |
| Audit, stock-movement, price-history and sales records cannot be edited or deleted | database triggers in `migrations/` (a payment's status is the one thing allowed to change) |
| Stock never goes negative, even under concurrent requests | conditional `UPDATE ... WHERE balance + delta >= 0` |
| A retried sale or return happens once | `Idempotency-Key` plus an advisory lock |
