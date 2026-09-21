#!/usr/bin/env bash
# Verify feed vs submission NIC queues. Fail closed.
# Exit 0 = PASS, 1 = FAIL (Linux but not applied), 2 = ABSENT (no /sys).
# Never exit 0 when /sys is missing.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONF="${ROOT}/queues.conf"
APPLIED="${ROOT}/state.applied"

echo "liq-net-queues: verify"

if [[ ! -d /sys ]]; then
  echo "ABSENT: /sys is not a directory — cannot evaluate NIC queues"
  exit 2
fi

if [[ ! -d /sys/class/net ]]; then
  echo "ABSENT: /sys/class/net missing"
  exit 2
fi

if [[ ! -f "${APPLIED}" ]]; then
  echo "FAIL: ${APPLIED} missing — queues.sh has not succeeded (not PASS)"
  exit 1
fi

IFACE=""
SUBMIT_Q=""
FEED_Q=""
while IFS= read -r line || [[ -n "${line}" ]]; do
  case "${line}" in
    ''|\#*) continue ;;
  esac
  key="${line%%=*}"
  val="${line#*=}"
  case "${key}" in
    iface) IFACE="${val}" ;;
    submit_tx_queue) SUBMIT_Q="${val}" ;;
    feed_tx_queue) FEED_Q="${val}" ;;
  esac
done < "${APPLIED}"

if [[ -z "${IFACE}" || "${IFACE}" == "CHANGE_ME" ]]; then
  echo "FAIL: state.applied iface is empty or CHANGE_ME"
  exit 1
fi

if [[ "${SUBMIT_Q}" == "${FEED_Q}" || -z "${SUBMIT_Q}" || -z "${FEED_Q}" ]]; then
  echo "FAIL: submit and feed queues must be distinct and set"
  exit 1
fi

DEV="/sys/class/net/${IFACE}"
if [[ ! -d "${DEV}" ]]; then
  echo "FAIL: ${DEV} missing"
  exit 1
fi

if [[ ! -d "${DEV}/queues/tx-${SUBMIT_Q}" ]]; then
  echo "FAIL: submit tx-${SUBMIT_Q} missing under ${DEV}/queues"
  exit 1
fi

if [[ ! -d "${DEV}/queues/tx-${FEED_Q}" ]]; then
  echo "FAIL: feed tx-${FEED_Q} missing under ${DEV}/queues"
  exit 1
fi

SUBMIT_XPS="${DEV}/queues/tx-${SUBMIT_Q}/xps_cpus"
FEED_XPS="${DEV}/queues/tx-${FEED_Q}/xps_cpus"
if [[ -f "${SUBMIT_XPS}" && -f "${FEED_XPS}" ]]; then
  S="$(tr -d ' \n' < "${SUBMIT_XPS}")"
  F="$(tr -d ' \n' < "${FEED_XPS}")"
  if [[ -z "${S}" || -z "${F}" ]]; then
    echo "FAIL: xps_cpus empty on submit or feed queue"
    exit 1
  fi
  if [[ "${S}" == "${F}" ]]; then
    echo "FAIL: submit and feed xps_cpus are identical — queues are not separated"
    exit 1
  fi
else
  echo "FAIL: xps_cpus not present for configured queues"
  exit 1
fi

# Conf file may still say CHANGE_ME; applied state is the source of truth.
if [[ -f "${CONF}" ]]; then
  :
fi

echo "PASS: iface=${IFACE} submit=tx-${SUBMIT_Q} feed=tx-${FEED_Q}"
exit 0
