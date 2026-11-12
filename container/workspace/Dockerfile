# May workspace image: the thin layer over the shared base that carries the
# container's tools — the managed heel binary, the `tasks` command and the
# aither baseline tools staged at /opt/aither/tools/<cache key>/bin by CI.
ARG BASE
FROM ${BASE}

# CI stages these into the build context; all are generated, never committed.
COPY heel /usr/local/bin/heel
COPY tasks /usr/local/bin/tasks
COPY tools/ /opt/aither/tools/

# Entrypoint generates playwright-cli.json from BROWSER_CDP_ENDPOINT
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod 0755 /usr/local/bin/heel /usr/local/bin/tasks /usr/local/bin/entrypoint.sh

WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["sleep", "infinity"]
