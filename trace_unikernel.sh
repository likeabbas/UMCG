#!/bin/bash

# Create log directory
LOG_DIR="/tmp/umcg_logs"
mkdir -p $LOG_DIR
TIMESTAMP=$(date +%Y%m%d_%H%M%S)

# Set up logging files
MAIN_LOG="${LOG_DIR}/umcg_${TIMESTAMP}.log"
ERROR_LOG="${LOG_DIR}/umcg_errors_${TIMESTAMP}.log"

# Run with minimal tracing that's less likely to interfere with the unikernel
strace -f -tt \
    -e trace=syscall \
    -e signal=none \
    -s 0 \
    -o "${LOG_DIR}/minimal_trace_${TIMESTAMP}.log" \
    ops run -c config.json target/x86_64-unknown-linux-musl/release/UMCG \
    2> >(tee "${ERROR_LOG}") \
    1> >(tee "${MAIN_LOG}")

# Save exit status
EXIT_CODE=$?

# Log summary
echo "=== Execution Summary ===" > "${LOG_DIR}/summary_${TIMESTAMP}.log"
echo "Exit Code: ${EXIT_CODE}" >> "${LOG_DIR}/summary_${TIMESTAMP}.log"
echo "Runtime: $(date)" >> "${LOG_DIR}/summary_${TIMESTAMP}.log"

# Print important syscall errors only
echo "=== Important Syscall Errors ===" >> "${LOG_DIR}/summary_${TIMESTAMP}.log"
grep "= -1 E" "${LOG_DIR}/minimal_trace_${TIMESTAMP}.log" >> "${LOG_DIR}/summary_${TIMESTAMP}.log"

echo "Logs available in ${LOG_DIR}"
echo "Main log: ${MAIN_LOG}"
echo "Error log: ${ERROR_LOG}"
echo "Summary: ${LOG_DIR}/summary_${TIMESTAMP}.log"
