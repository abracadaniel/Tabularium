# Roadmap

Features DataGrip has that Tabularium does not yet, and parts that exist but are
not fully verified.

## Databases

| Database | Status | Likely approach |
|---|---|---|
| Oracle | missing | `oracle` crate (needs Oracle Instant Client) or JDBC bridge |
| IBM DB2 | missing | JDBC bridge |
| Snowflake | missing | REST/SQL API in Rust, or JDBC bridge |
| Google BigQuery | missing | REST API in Rust, or JDBC bridge |
| MongoDB | missing | official `mongodb` Rust driver (non-SQL UI needed) |
| Redis | missing | `redis` Rust crate (key browser UI needed) |
| Cassandra / ScyllaDB | missing | `scylla` Rust driver |
| Anything else with a JDBC driver | missing | generic JDBC bridge (see below) |

## Verification gaps

- SQL Server driver has unit tests only; never run against a real server.
- CockroachDB and Redshift introspection is best effort (catalog differences).
- SSH tunnels are not tested against a real SSH server.

## Driver features

- Query cancellation for SQL Server.
- `known_hosts` verification for SSH tunnels (host keys are currently accepted and
  their fingerprint logged).
- Postgres `RAISE NOTICE` and MySQL warnings are not captured as notices.
- DuckDB: no session list, FK actions always reported as NO ACTION, generated
  columns not detected.
- ClickHouse: primary keys are not unique, so data-editor edits may match more
  than one row; user-defined functions are not listed.

## IDE features

- Refactorings (rename a column/table across scripts), find usages.
- Version control integration and local file history.
- Custom data extractors and scripting (DataGrip's Groovy/JS extractors).
- Graphical query plans for databases other than PostgreSQL.

## JDBC bridge (planned)

Drivers don't have to be Rust. DataGrip reaches most databases through JDBC, and
Tabularium can too, as an optional fallback, without making Java a hard dependency.
Native Rust drivers stay the default wherever one exists.

### Approach: Java sidecar process (chosen)

- A small bundled Java program (`tabularium-jdbc.jar`) loads JDBC driver jars and
  talks to the app over stdin/stdout using a JSON protocol.
- On the Rust side it is one more driver behind the existing `Connection` trait,
  so the explorer, consoles, grid and tools work unchanged.
- A crashing JDBC driver kills only the sidecar, not the app.
- Driver jars are downloaded on demand from Maven Central and cached, like
  DataGrip's driver manager; users can also point at their own jars.
- Generic `java.sql.DatabaseMetaData` gives baseline introspection (schemas,
  tables, columns, keys, indexes) for any database; per-database catalog queries
  and DDL can be layered on where it matters.

Rejected alternative: embedding a JVM in the app via JNI. Fewer processes, but
finding the user's JVM is harder on each OS, and a driver crash would take the
whole app down.

### Costs

- Users need a Java 11+ runtime installed (detected via `JAVA_HOME` / `PATH`).
- Slower connection start-up and higher memory use than native drivers.
- Database-specific quirks (catalog queries, DDL, type mapping) still need
  per-database work for a polished experience.

### Order of work

1. JDBC sidecar and generic `Connection` implementation; driver download manager.
   This unlocks Oracle, DB2, Snowflake, BigQuery and anything with a JDBC driver.
2. Database-specific polish for Oracle and Snowflake (catalog queries, DDL).
3. Native Rust drivers where good ones exist: MongoDB (`mongodb`), Redis
   (`redis`), Cassandra/ScyllaDB (`scylla`). These need non-table UIs (document
   and key browsers).
4. Optional native replacements via HTTP APIs: Snowflake SQL API, BigQuery REST,
   as done for ClickHouse.
