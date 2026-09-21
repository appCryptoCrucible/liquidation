#!/usr/bin/env bash
# Apply feed vs submission NIC queue separation (GUIDE 16 §5).
# Linux + ethtool only. This script never claims success on a host without /sys.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=queues.conf
CONF="${ROOT}/queues.conf"

echo "liq-net-queues: apply"

if [[ ! -d /sys ]]; then
  echo "ABSENT: /sys is not a directory — cannot apply ethtool (not Linux or no sysfs)"
  exit 2
fi

if [[ ! -d /sys/class/net ]]; then
  echo "ABSENT: /sys/class/net missing"
  exit 2
fi

if ! command -v ethtool >/dev/null 2>&1; then
  echo "FAIL: ethtool not on PATH"
  exit 1
fi

IFACE="${LIQ_NET_IFACE:-}"
SUBMIT_Q="${LIQ_NET_SUBMIT_TX:-}"
FEED_Q="${LIQ_NET_FEED_TX:-}"

if [[ -f "${CONF}" ]]; then
  while IFS= read -r line || [[ -n "${line}" ]]; do
    case "${line}" in
      ''|\#*) continue ;;
    esac
    key="${line%%=*}"
    val="${line#*=}"
    case "${key}" in
      iface) IFACE="${IFACE:-${val}}" ;;
      submit_tx_queue) SUBMIT_Q="${SUBMIT_Q:-${val}}" ;;
      feed_tx_queue) FEED_Q="${FEED_Q:-${val}}" ;;
    esac
  done < "${CONF}"
fi

if [[ -z "${IFACE}" || "${IFACE}" == "CHANGE_ME" ]]; then
  echo "FAIL: set LIQ_NET_IFACE or iface= in queues.conf to a real netdev"
  exit 1
fi

if [[ -z "${SUBMIT_Q}" || -z "${FEED_Q}" ]]; then
  echo "FAIL: submit_tx_queue and feed_tx_queue must be set"
  exit 1
fi

if [[ "${SUBMIT_Q}" == "${FEED_Q}" ]]; then
  echo "FAIL: feed and submit must be different tx queues"
  exit 1
fi

DEV="/sys/class/net/${IFACE}"
if [[ ! -d "${DEV}" ]]; then
  echo "FAIL: ${DEV} does not exist"
  exit 1
fi

# Combined / default queue counts vary by driver. Require at least two TX queues
# so feeds and submission can be steered apart. Do not guess a combined channel.
TX_N="$(find "${DEV}/queues" -maxdepth 1 -type d -name 'tx-*' 2>/dev/null | wc -l | tr -d ' ')"
if [[ "${TX_N}" -lt 2 ]]; then
  echo "FAIL: ${IFACE} has ${TX_N} tx queues; need ≥2 before ethtool steering"
  exit 1
fi

# Combined channels: ask for at least 2 RX + 2 TX. Driver may refuse; fail closed.
if ! ethtool -L "${IFACE}" combined 2; then
  if ! ethtool -L "${IFACE}" rx 2 tx 2; then
    echo "FAIL: ethtool -L could not create ≥2 queues on ${IFACE}"
    exit 1
  fi
fi

# XPS: submission queue on one set of CPUs, feed queue on another.
# Operator supplies masks (hex). Empty → fail; do not invent affinity.
SUBMIT_XPS="${LIQ_NET_SUBMIT_XPS:-}"
FEED_XPS="${LIQ_NET_FEED_XPS:-}"
if [[ -z "${SUBMIT_XPS}" || -z "${FEED_XPS}" ]]; then
  echo "FAIL: set LIQ_NET_SUBMIT_XPS and LIQ_NET_FEED_XPS (hex cpu masks). No guessed affinity."
  exit 1
fi

SUBMIT_XPS_PATH="${DEV}/queues/tx-${SUBMIT_Q}/xps_cpus"
FEED_XPS_PATH="${DEV}/queues/tx-${FEED_Q}/xps_cpus"
if [[ ! -f "${SUBMIT_XPS_PATH}" || ! -f "${FEED_XPS_PATH}" ]]; then
  echo "FAIL: missing xps_cpus for tx-${SUBMIT_Q} / tx-${FEED_Q}"
  exit 1
fi

printf '%s' "${SUBMIT_XPS}" > "${SUBMIT_XPS_PATH}"
printf '%s' "${FEED_XPS}" > "${FEED_XPS_PATH}"

# ntuple: operator must supply the actual builder / CEX tuples. No invented IPs.
# Example (not executed unless LIQ_NET_APPLY_NTUPLE=1 and the rule files exist):
#   ethtool -N $IFACE flow-type tcp4 dst-ip $BUILDER_IP dst-port 443 action $SUBMIT_Q
if [[ "${LIQ_NET_APPLY_NTUPLE:-0}" == "1" ]]; then
  NTUPLE="${ROOT}/ntuple.rules"
  if [[ ! -f "${NTUPLE}" ]]; then
    echo "FAIL: LIQ_NET_APPLY_NTUPLE=1 but ${NTUPLE} is missing"
    exit 1
  fi
  while IFS= read -r rule || [[ -n "${rule}" ]]; do
    case "${rule}" in
      ''|\#*) continue ;;
    esac
    # Each line is ethtool -N arguments after the device name.
    # shellcheck disable=SC2086
    ethtool -N "${IFACE}" ${rule}
  done < "${NTUPLE}"
fi

{
  echo "iface=${IFACE}"
  echo "submit_tx_queue=${SUBMIT_Q}"
  echo "feed_tx_queue=${FEED_Q}"
  echo "submit_xps=${SUBMIT_XPS}"
  echo "feed_xps=${FEED_XPS}"
} > "${ROOT}/state.applied"

echo "applied: iface=${IFACE} submit=tx-${SUBMIT_Q} feed=tx-${FEED_Q}"
echo "verify with ops/net/verify-queues.sh"
