# The reference dashboard

`shep.json` is a starting Grafana dashboard for the metrics dog: dog health,
the shepherd itself, flock status, and CPU, memory, and restarts per sheep.
Treat it as a base to edit, not a finished ops setup.

## Importing it

1. In Grafana, go to **Dashboards -> New -> Import**.
2. Upload `shep.json`, or paste its contents into the import box.
3. When Grafana asks for the `Prometheus` input, point it at whichever
   Prometheus instance scrapes the metrics dog.
4. Import.

The dashboard ships with no pinned UID, so it imports cleanly even into a
Grafana that already has an older copy from a previous version.

## What it expects

A Prometheus datasource scraping `/metrics` on the metrics dog, which binds
loopback `127.0.0.1:9615` by default (`[dog.metrics] bind` to change it). If
the dog isn't running yet, start it with `shep enable metrics`.

## What it can't show

Only what the metrics dog renders. An empty panel usually isn't a scrape
problem; it means that metric doesn't exist yet. Check the "Dog health"
panel first: if it's red, every panel below it is stale, and the gap is the
dog being down, not a missing metric.
