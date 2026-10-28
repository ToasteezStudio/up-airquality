-- TimescaleDB schema for UP-AirQuality readings.
-- Run on your PostgreSQL + TimescaleDB server against a dedicated `airquality` DB.
--   createdb airquality   (or CREATE DATABASE airquality;)
--   psql -d airquality -f schema.sql

CREATE EXTENSION IF NOT EXISTS timescaledb;

CREATE TABLE IF NOT EXISTS air_quality (
    ts          timestamptz NOT NULL DEFAULT now(),
    source      text,
    aqi         double precision,
    co2         double precision,
    humidity    double precision,
    temperature double precision,
    pm1p0       double precision,
    pm2p5       double precision,
    pm4p0       double precision,
    pm10p0      double precision,
    voc         double precision,
    nox         double precision,
    tvoc        double precision,
    vape        double precision
);

SELECT create_hypertable('air_quality', 'ts', if_not_exists => TRUE);

-- 5-minute rollup for fast long-range charts.
CREATE MATERIALIZED VIEW IF NOT EXISTS air_quality_5m
WITH (timescaledb.continuous) AS
SELECT time_bucket('5 minutes', ts) AS bucket,
       avg(aqi)         AS aqi,
       avg(co2)         AS co2,
       avg(humidity)    AS humidity,
       avg(temperature) AS temperature,
       avg(pm1p0)       AS pm1p0,
       avg(pm2p5)       AS pm2p5,
       avg(pm4p0)       AS pm4p0,
       avg(pm10p0)      AS pm10p0,
       avg(voc)         AS voc,
       avg(nox)         AS nox,
       avg(tvoc)        AS tvoc,
       max(vape)        AS vape
FROM air_quality
GROUP BY bucket
WITH NO DATA;

SELECT add_continuous_aggregate_policy('air_quality_5m',
    start_offset      => INTERVAL '3 hours',
    end_offset        => INTERVAL '5 minutes',
    schedule_interval => INTERVAL '5 minutes',
    if_not_exists     => TRUE);

-- Keep raw rows 90 days; the 5-min aggregate persists for long history.
SELECT add_retention_policy('air_quality', INTERVAL '90 days', if_not_exists => TRUE);

-- Least-privilege writer role used by the console (set a real password):
--   CREATE ROLE upaq LOGIN PASSWORD 'CHANGE_ME';
--   GRANT CONNECT ON DATABASE airquality TO upaq;
--   GRANT INSERT, SELECT ON air_quality TO upaq;
-- Grafana read-only role:
--   CREATE ROLE grafana_ro LOGIN PASSWORD 'CHANGE_ME';
--   GRANT CONNECT ON DATABASE airquality TO grafana_ro;
--   GRANT SELECT ON air_quality, air_quality_5m TO grafana_ro;
