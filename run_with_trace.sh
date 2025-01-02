#!/bin/bash

# Create log directory
LOG_DIR="/tmp/umcg_logs"
mkdir -p $LOG_DIR

# Timestamp for log files
TIMESTAMP=$(date +%Y%m%d_%H%M%S)

# Run the application with various tracers
strace -f -tt \
    -e trace=process,network,signal,desc,memory,ipc \
    -s 1024 \
    -o "${LOG_DIR}/syscalls_${TIMESTAMP}.log" \
    ops run -c config.json target/x86_64-unknown-linux-musl/release/UMCG \
    2> >(tee "${LOG_DIR}/stderr_${TIMESTAMP}.log") \
    1> >(tee "${LOG_DIR}/stdout_${TIMESTAMP}.log")

# Save important system state at time of exit
echo "=== System State at Exit ===" > "${LOG_DIR}/system_state_${TIMESTAMP}.log"
echo "Process Tree:" >> "${LOG_DIR}/system_state_${TIMESTAMP}.log"
pstree -p >> "${LOG_DIR}/system_state_${TIMESTAMP}.log"
echo -e "\nSystem Messages:" >> "${LOG_DIR}/system_state_${TIMESTAMP}.log"
tail -n 50 /var/log/messages >> "${LOG_DIR}/system_state_${TIMESTAMP}.log" 2>/dev/null
dmesg | tail -n 50 >> "${LOG_DIR}/system_state_${TIMESTAMP}.log" 2>/dev/null
