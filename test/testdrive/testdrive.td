
# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

# Tests for testdrive itself.

# Uncomment to test that timeouts happen in the time desired.
#
# Note that the duration string format can be anything parsable
# by the parse_duration create
#
# $ set-sql-timeout duration=2minutes
# $ set-sql-timeout duration=default
# > select * from nonexistent

# Test that hashing rows works and is consistent.

> CREATE VIEW v AS VALUES (1, 'foo'), (2, 'bar'), (3, 'foo'), (1, 'bar')

> SELECT * FROM v
4 values hashing to 7dd470c8470b085df13552e191a244ab

> VALUES ('row', 1), ('row', 2)
row 1
# inline comment
row 2

# Test DATE , TIME, TIMESTAMP output

> CREATE TABLE t1 (f1 DATE, f2 TIME, f3 TIMESTAMP)

> INSERT INTO t1 VALUES ('2011-11-11', '11:11:11', '2011-11-11 11:11:11')

> SELECT * FROM t1
"2011-11-11" "11:11:11" "2011-11-11 11:11:11"

# Test set-regex

$ set-regex match=u\d+ replacement=UID

? EXPLAIN SELECT * FROM t1 AS a1, t1 AS a2 WHERE a1.f1 IS NOT NULL;
Source materialize.public.t1 (UID):
| Project (#0..=#2)

Query:
%0 =
| Get materialize.public.t1 (UID)
| Filter !(isnull(#0))
| ArrangeBy ()

%1 =
| Get materialize.public.t1 (UID)

%2 =
| Join %0 %1
| | implementation = Differential %1 %0.()

! SELECT * FROM u1234;
contains:unknown catalog item 'UID'

# Exclude FETCH from the retry logic

> CREATE MATERIALIZED VIEW v1 AS VALUES (1),(2),(3);

> BEGIN

> DECLARE c CURSOR FOR TAIL v1 AS OF 18446744073709551615;

> FETCH 4 c WITH (timeout='10s');
18446744073709551615 1 1
18446744073709551615 1 2
18446744073709551615 1 3

> COMMIT

# kafka-verify sort-messages

> CREATE VIEW sort_messages (a) AS VALUES (2),(1),(3);

> CREATE SINK sort_messages_sink FROM sort_messages
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'sort-messages-sink'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

$ kafka-verify format=avro sink=materialize.public.sort_messages_sink sort-messages=true
{"before": null, "after": {"row": {"a": 1}}}
{"before": null, "after": {"row": {"a": 2}}}
{"before": null, "after": {"row": {"a": 3}}}

# Use $ postgresql-execute and ${testdrive.materialize_addr}

$ postgres-execute connection=postgres://materialize:materialize@${testdrive.materialized-addr}
CREATE TABLE postgres_execute (f1 INTEGER);
INSERT INTO postgres_execute VALUES (123);

> SELECT * FROM postgres_execute;
123

# http-request

$ http-request method=GET url=${testdrive.schema-registry-url}schemas/types

# kafka-ingest repeat

$ set kafka-ingest-repeat={
        "type" : "record",
        "name" : "test",
        "fields" : [
            {"name":"f1", "type":"string"}
        ]
    }

$ kafka-create-topic topic=kafka-ingest-repeat

$ kafka-ingest format=avro topic=kafka-ingest-repeat schema=${kafka-ingest-repeat} publish=true repeat=2
{"f1": "fish"}

> CREATE MATERIALIZED SOURCE kafka_ingest_repeat_input
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC
  'testdrive-kafka-ingest-repeat-${testdrive.seed}'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
  ENVELOPE NONE

> SELECT * FROM kafka_ingest_repeat_input;
fish
fish

# kafka-ingest repeat with ${kafka-ingest.iteration}

$ kafka-ingest format=avro topic=kafka-ingest-repeat schema=${kafka-ingest-repeat} publish=true repeat=2
{"f1": "${kafka-ingest.iteration}"}

> SELECT * FROM kafka_ingest_repeat_input;
0
1
fish
fish

# kafka-ingest with no explicit 'partition' argument should spread the records evenly across the partitions

$ set kafka-ingest-no-partition-key={"type": "string"}
$ set kafka-ingest-no-partition-value={"type": "record", "name": "r", "fields": [{"name": "a", "type": "string"}]}

$ kafka-create-topic topic=kafka-ingest-no-partition partitions=2

$ kafka-ingest format=avro topic=kafka-ingest-no-partition key-format=avro key-schema=${kafka-ingest-no-partition-key} schema=${kafka-ingest-no-partition-value} publish=true
"a" {"a": "a"}
"b" {"a": "b"}
"c" {"a": "c"}
"d" {"a": "d"}
"e" {"a": "e"}
"f" {"a": "f"}
"g" {"a": "g"}
"h" {"a": "h"}

> CREATE MATERIALIZED SOURCE kafka_ingest_no_partition
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-kafka-ingest-no-partition-${testdrive.seed}'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
  ENVELOPE NONE

> SELECT COUNT(*) = 8 FROM kafka_ingest_no_partition;
true

# kafka-verify with regexp (the set-regexp from above is used)

> CREATE VIEW kafka_verify_regexp (a) AS VALUES ('u123'), ('u234');

> CREATE SINK kafka_verify_regexp_sink FROM kafka_verify_regexp
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'kafka-verify-regexp-sink'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

$ kafka-verify format=avro sink=materialize.public.kafka_verify_regexp_sink sort-messages=true
{"before": null, "after": {"row": {"a": "UID"}}}
{"before": null, "after": {"row": {"a": "UID"}}}

# $ postgresql-connect

> CREATE TABLE postgres_connect (f1 INTEGER);

$ postgres-connect name=conn1 url=postgres://materialize:materialize@${testdrive.materialized-addr}

$ postgres-execute connection=conn1
BEGIN;
INSERT INTO postgres_connect VALUES (1);

# Table is still empty, the transaction we just started is not committed yet
> SELECT COUNT(*) = 0 FROM postgres_connect;
true

$ postgres-execute connection=conn1
INSERT INTO postgres_connect VALUES (2);
COMMIT;

> SELECT COUNT(*) = 2 FROM postgres_connect;
true
