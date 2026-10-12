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

## JDBC bridge (proposal)

DataGrip reaches most databases through JDBC. Tabularium could do the same as an
optional fallback without making Java a hard dependency:

- A small Java sidecar process (bundled jar) that loads JDBC driver jars and speaks
  a JSON protocol over stdin/stdout, implementing the existing `Connection` trait
  on the Rust side.
- Driver jars downloaded on demand from Maven Central, like DataGrip's driver
  manager.
- Requires a Java 11+ runtime on the user's machine; native Rust drivers stay the
  default wherever one exists.
