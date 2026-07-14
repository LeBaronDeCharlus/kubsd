#!/bin/sh
#
# Generates the private CA and per-identity leaf certificates Milestone
# 12's mutual TLS uses. Every leaf certificate is signed by one CA and
# gets both serverAuth and clientAuth extended key usage, since a node's
# (and the control plane's own) single identity certificate is used both
# ways: server when accepting an inbound connection, client when dialing
# out. Validity defaults to 10 years, overridable with a trailing
# --days N; this project's own test fixtures use --days 36500 (~100
# years) so testdata/tls/ never needs regenerating.
#
# Usage:
#   ./scripts/gen-certs.sh init
#   ./scripts/gen-certs.sh node <name> <ip-address> [--days N]
#   ./scripts/gen-certs.sh client <name> [--days N]
#
# Output goes to $GEN_CERTS_OUT_DIR (default: ./certs).

set -eu

OUT_DIR="${GEN_CERTS_OUT_DIR:-certs}"
DEFAULT_DAYS=3650

mkdir -p "$OUT_DIR"

fail() {
    echo "gen-certs: $*" >&2
    exit 1
}

cmd="${1:-}"
[ -n "$cmd" ] || fail "usage: $0 <init|node|client> ..."
shift

issue_leaf() {
    name="$1"
    days="$2"
    san_line="$3"

    openssl genrsa -out "$OUT_DIR/$name.key" 4096
    openssl req -new -key "$OUT_DIR/$name.key" -subj "/CN=$name" -out "$OUT_DIR/$name.csr"

    ext_file=$(mktemp)
    trap 'rm -f "$ext_file"' EXIT
    if [ -n "$san_line" ]; then
        printf 'subjectAltName = %s\nextendedKeyUsage = serverAuth, clientAuth\n' "$san_line" > "$ext_file"
    else
        printf 'extendedKeyUsage = serverAuth, clientAuth\n' > "$ext_file"
    fi

    openssl x509 -req -in "$OUT_DIR/$name.csr" -CA "$OUT_DIR/ca.crt" -CAkey "$OUT_DIR/ca.key" \
        -CAcreateserial -out "$OUT_DIR/$name.crt" -days "$days" -sha256 -extfile "$ext_file"
    rm -f "$OUT_DIR/$name.csr" "$ext_file"
    trap - EXIT
}

case "$cmd" in
    init)
        [ -f "$OUT_DIR/ca.key" ] && fail "$OUT_DIR/ca.key already exists, refusing to overwrite"
        openssl genrsa -out "$OUT_DIR/ca.key" 4096
        openssl req -x509 -new -nodes -key "$OUT_DIR/ca.key" -sha256 -days "$DEFAULT_DAYS" \
            -subj "/CN=keel-cluster-ca" -out "$OUT_DIR/ca.crt"
        echo "gen-certs: wrote $OUT_DIR/ca.crt and $OUT_DIR/ca.key"
        ;;
    node)
        name="${1:-}"
        addr="${2:-}"
        [ -n "$name" ] && [ -n "$addr" ] || fail "usage: $0 node <name> <ip-address> [--days N]"
        shift 2
        days="$DEFAULT_DAYS"
        if [ "${1:-}" = "--days" ]; then
            days="${2:-}"
            [ -n "$days" ] || fail "--days requires a value"
        fi
        [ -f "$OUT_DIR/ca.key" ] || fail "run '$0 init' first"
        issue_leaf "$name" "$days" "IP:$addr"
        echo "gen-certs: wrote $OUT_DIR/$name.crt and $OUT_DIR/$name.key (SAN=IP:$addr)"
        ;;
    client)
        name="${1:-}"
        [ -n "$name" ] || fail "usage: $0 client <name> [--days N]"
        shift
        days="$DEFAULT_DAYS"
        if [ "${1:-}" = "--days" ]; then
            days="${2:-}"
            [ -n "$days" ] || fail "--days requires a value"
        fi
        [ -f "$OUT_DIR/ca.key" ] || fail "run '$0 init' first"
        issue_leaf "$name" "$days" ""
        echo "gen-certs: wrote $OUT_DIR/$name.crt and $OUT_DIR/$name.key"
        ;;
    *)
        fail "unknown subcommand: $cmd (expected init|node|client)"
        ;;
esac
