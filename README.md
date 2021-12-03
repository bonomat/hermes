# Itchy Sats

[![Bors enabled](https://bors.tech/images/badge_small.svg)](https://app.bors.tech/repositories/39253)

CFD trading on Bitcoin.

Details coming soon.

## Quickstart

All the components can be started at once by running the following script:

```bash
./start_all.sh
```

Note: Before first run, you need to run `cd maker-frontend; yarn install; cd../taker-frontend; yarn install` command to ensure that all dependencies get
installed.

The script combines the logs from all binaries inside a single terminal so it
might not be ideal for all cases, but it is convenient for quick regression testing.

Pressing `Ctrl + c` once stops all the processes.

The script also enables backtraces by setting `RUST_BACKTRACE=1` env variable.

## Starting the maker and taker daemon

A working example of starting both daemons with all the required command-line parameters can be found
[here](https://github.com/itchysats/itchysats/blob/master/start_all.sh#L8)

The maker and taker frontend depend on the respective daemon running.

## Starting the maker and taker frontend

We use a separate react projects for hosting taker and maker frontends.

At the moment you will need a browser extension to allow CORS headers like `CORS Everywhere` ([Firefox Extension](https://addons.mozilla.org/en-US/firefox/addon/cors-everywhere/)) to use the frontends.

### Taker

```bash
cd taker-frontend
yarn install
yarn dev
```

### Maker

```bash
cd maker-frontend
yarn install
yarn dev
```

### Linting

To run eslint, use:

```bash
cd maker-frontend && yarn run eslint
cd taker-frontend && yarn run eslint
```
