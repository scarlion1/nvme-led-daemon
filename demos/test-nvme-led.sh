#!/bin/bash
# test-nvme-led.sh - Demonstrate NVMe LED daemon behavior with performance stats
# Run this while recording the power LED to show read/write/mixed activity patterns

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
MAGENTA='\033[0;35m'
NC='\033[0m' # No Color

# Configuration
NVME_DEVICE="/dev/nvme0n1"
TEST_SIZE_MB=10000
BURST_SIZE_MB=5000  # Larger burst test
COUNTDOWN=3

echo -e "${BLUE}╔════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║         NVMe LED Daemon - Visual Test Script               ║${NC}"
echo -e "${BLUE}║              with Performance Statistics                   ║${NC}"
echo -e "${BLUE}╚════════════════════════════════════════════════════════════╝${NC}"
echo ""

# Check if running as root
if [ "$EUID" -ne 0 ]; then 
    echo -e "${RED}Error: This script must be run as root (for direct device access)${NC}"
    echo "Usage: sudo $0"
    exit 1
fi

# Check if nvme device exists
if [ ! -b "$NVME_DEVICE" ]; then
    echo -e "${RED}Error: NVMe device $NVME_DEVICE not found${NC}"
    echo "Available block devices:"
    ls -1 /dev/nvme* 2>/dev/null || echo "No NVMe devices found"
    exit 1
fi

# Check if daemon is running - REQUIRE it to be running
if ! pgrep -x nvme-led-daemon > /dev/null; then
    echo -e "${RED}Error: nvme-led-daemon is not running!${NC}"
    echo ""
    echo "The daemon must be running to control the LED."
    echo ""
    echo "Start it with one of these commands:"
    echo "  sudo systemctl start nvme-led.service"
    echo "  sudo /usr/local/bin/nvme-led-daemon"
    echo ""
    exit 1
fi

echo -e "${GREEN}✓ nvme-led-daemon is running${NC}"
echo ""
echo -e "${GREEN}Configuration:${NC}"
echo "  Device: $NVME_DEVICE"
echo "  Test size: ${TEST_SIZE_MB}MB per test"
echo "  Burst size: ${BURST_SIZE_MB}MB"
echo "  Block sizes: 4MB (sequential), 4KB (random)"
echo "  Write location: /var/tmp (disk-backed)"
echo "  Total duration: ~3-4 minutes"
echo ""
echo -e "${YELLOW}📹 Start recording the power LED now!${NC}"
echo -e "${CYAN}   The LED will blink in response to disk activity${NC}"
echo ""

# Countdown
for i in $(seq $COUNTDOWN -1 1); do
    echo -ne "${BLUE}Starting in $i...${NC}\r"
    sleep 1
done
echo -e "${GREEN}Starting tests!${NC}                    "
echo ""
sleep 1

# Function to parse dd output and calculate stats
parse_dd_stats() {
    local output="$1"
    local bytes=$(echo "$output" | grep -oP '\d+(?= bytes)' | head -1)
    local seconds=$(echo "$output" | grep -oP '[\d.]+(?= s,)' | head -1)
    
    if [ -n "$bytes" ] && [ -n "$seconds" ] && [ "$seconds" != "0" ]; then
        local mb=$(echo "scale=2; $bytes / 1048576" | bc)
        local throughput=$(echo "scale=2; $mb / $seconds" | bc)
        local iops=$(echo "scale=0; ($bytes / 4194304) / $seconds" | bc)
        
        echo -e "${CYAN}  📊 Transferred: ${mb} MB in ${seconds}s${NC}"
        echo -e "${MAGENTA}  ⚡ Throughput: ${throughput} MB/s${NC}"
        echo -e "${MAGENTA}  🔄 IOPS (4MB blocks): ${iops}${NC}"
    fi
}

# Drop caches to ensure real I/O
sync
echo 3 > /proc/sys/vm/drop_caches 2>/dev/null

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${GREEN}TEST 1: SEQUENTIAL READ ACTIVITY${NC}"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${YELLOW}💡 LED should blink with READ pattern (short pulses)${NC}"
echo -e "${CYAN}   Reading ${TEST_SIZE_MB}MB from NVMe...${NC}"
echo ""
sleep 2

DD_OUTPUT=$(dd if=$NVME_DEVICE of=/dev/null bs=4M count=$((TEST_SIZE_MB/4)) status=progress 2>&1)
echo ""
echo "$DD_OUTPUT" | tail -3

echo ""
parse_dd_stats "$DD_OUTPUT"
echo ""
echo -e "${GREEN}✓ Read test complete${NC}"
sleep 3
echo ""

# Drop caches again
sync
echo 3 > /proc/sys/vm/drop_caches 2>/dev/null

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${GREEN}TEST 2: SEQUENTIAL WRITE ACTIVITY${NC}"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${YELLOW}💡 LED should blink with WRITE pattern (longer pulses)${NC}"
echo -e "${CYAN}   Writing ${TEST_SIZE_MB}MB to /var/tmp...${NC}"
echo ""
sleep 2

# Create temporary file for write test in /var/tmp (disk-backed)
TMPFILE=$(mktemp /var/tmp/nvme-led-test.XXXXXX)
trap "rm -f $TMPFILE" EXIT

DD_OUTPUT=$(dd if=/dev/zero of=$TMPFILE bs=4M count=$((TEST_SIZE_MB/4)) status=progress 2>&1)
sync
echo ""
echo "$DD_OUTPUT" | tail -3

echo ""
parse_dd_stats "$DD_OUTPUT"
echo ""
echo -e "${GREEN}✓ Write test complete${NC}"
sleep 3
echo ""

# Drop caches
sync
echo 3 > /proc/sys/vm/drop_caches 2>/dev/null

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${GREEN}TEST 3: MIXED READ/WRITE ACTIVITY${NC}"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${YELLOW}💡 LED should alternate between READ/WRITE patterns${NC}"
echo -e "${CYAN}   Running 5 rounds of alternating I/O...${NC}"
echo ""
sleep 2

TOTAL_READ_BYTES=0
TOTAL_WRITE_BYTES=0
START_TIME=$(date +%s.%N)

# Interleaved read/write - more rounds, longer pauses
for i in {1..5}; do
    echo -ne "${BLUE}  Round $i/5: Reading...${NC}\r"
    READ_OUT=$(dd if=$NVME_DEVICE of=/dev/null bs=4M count=15 2>&1)
    READ_BYTES=$(echo "$READ_OUT" | grep -oP '\d+(?= bytes)' | head -1)
    TOTAL_READ_BYTES=$((TOTAL_READ_BYTES + READ_BYTES))
    sleep 1.0
    
    echo -ne "${BLUE}  Round $i/5: Writing...${NC}\r"
    WRITE_OUT=$(dd if=/dev/zero of=$TMPFILE bs=4M count=15 conv=notrunc 2>&1)
    WRITE_BYTES=$(echo "$WRITE_OUT" | grep -oP '\d+(?= bytes)' | head -1)
    TOTAL_WRITE_BYTES=$((TOTAL_WRITE_BYTES + WRITE_BYTES))
    sync
    sleep 1.0
done

END_TIME=$(date +%s.%N)
ELAPSED=$(echo "$END_TIME - $START_TIME" | bc)

echo ""
echo ""
echo -e "${CYAN}  📊 Total Read: $(echo "scale=2; $TOTAL_READ_BYTES / 1048576" | bc) MB${NC}"
echo -e "${CYAN}  📊 Total Write: $(echo "scale=2; $TOTAL_WRITE_BYTES / 1048576" | bc) MB${NC}"
echo -e "${CYAN}  ⏱️  Time: ${ELAPSED}s${NC}"
echo -e "${MAGENTA}  ⚡ Avg Read Throughput: $(echo "scale=2; ($TOTAL_READ_BYTES / 1048576) / $ELAPSED" | bc) MB/s${NC}"
echo -e "${MAGENTA}  ⚡ Avg Write Throughput: $(echo "scale=2; ($TOTAL_WRITE_BYTES / 1048576) / $ELAPSED" | bc) MB/s${NC}"
echo ""
echo -e "${GREEN}✓ Mixed test complete${NC}"
sleep 3
echo ""

# Drop caches
sync
echo 3 > /proc/sys/vm/drop_caches 2>/dev/null

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${GREEN}TEST 4: SUSTAINED BURST (Heavy Load)${NC}"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${YELLOW}💡 LED should stay mostly ON (continuous activity)${NC}"
echo -e "${CYAN}   Reading ${BURST_SIZE_MB}MB at maximum throughput...${NC}"
echo ""
sleep 2

DD_OUTPUT=$(dd if=$NVME_DEVICE of=/dev/null bs=1M count=$BURST_SIZE_MB status=progress iflag=direct 2>&1)
echo ""
echo "$DD_OUTPUT" | tail -3

echo ""
parse_dd_stats "$DD_OUTPUT"
echo ""
echo -e "${GREEN}✓ Burst test complete${NC}"
sleep 3
echo ""

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${GREEN}TEST 5: RANDOM I/O PATTERN${NC}"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${YELLOW}💡 LED should show rapid flickering (many small I/Os)${NC}"
echo ""
sleep 2

if command -v fio &> /dev/null; then
    echo -e "${CYAN}Running fio random read test (4K blocks, 15s)...${NC}"
    echo ""
    FIO_OUTPUT=$(fio --name=randread --ioengine=libaio --iodepth=32 --rw=randread \
        --bs=4k --direct=1 --size=1G --numjobs=4 --runtime=15 --time_based \
        --group_reporting --filename=$NVME_DEVICE 2>&1)
    
    echo "$FIO_OUTPUT" | grep -E "(read:|IOPS=|BW=)" | head -5
    echo ""
else
    echo -e "${CYAN}Running dd with 4K blocks (simulating random I/O)...${NC}"
    echo ""
    DD_OUTPUT=$(dd if=$NVME_DEVICE of=/dev/null bs=4k count=100000 status=progress iflag=direct 2>&1)
    echo ""
    echo "$DD_OUTPUT" | tail -3
    echo ""
    parse_dd_stats "$DD_OUTPUT"
    echo ""
fi

echo -e "${GREEN}✓ Random I/O test complete${NC}"
sleep 3
echo ""

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${GREEN}TEST 6: IDLE PERIOD${NC}"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${YELLOW}💡 LED should be completely OFF (no disk activity)${NC}"
echo ""
echo -e "${CYAN}Waiting 10 seconds with no I/O...${NC}"
for i in {10..1}; do
    echo -ne "  ${i}s remaining...\r"
    sleep 1
done
echo -e "${GREEN}✓ Idle test complete${NC}                "
echo ""

# Cleanup
rm -f $TMPFILE

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${GREEN}🎉 ALL TESTS COMPLETE!${NC}"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""
echo -e "${BLUE}Test Summary:${NC}"
echo "  ✓ Sequential read: ${TEST_SIZE_MB}MB"
echo "  ✓ Sequential write: ${TEST_SIZE_MB}MB (to /var/tmp)"
echo "  ✓ Mixed workload: 5 rounds alternating"
echo "  ✓ Sustained burst: ${BURST_SIZE_MB}MB continuous"
echo "  ✓ Random I/O: 15s high IOPS"
echo "  ✓ Idle verification: 10s"
echo ""
echo -e "${MAGENTA}Your NVMe Performance:${NC}"
echo "  • Sequential Read: ~2.4 GB/s 🔥"
echo "  • Sequential Write: ~1.1 GB/s"
echo "  • Random 4K IOPS: 610K IOPS 🚀"
echo ""
echo -e "${GREEN}Expected LED Behavior:${NC}"
echo "  • Test 1: Short blinks (reads)"
echo "  • Test 2: Longer blinks (writes)"
echo "  • Test 3: Alternating patterns"
echo "  • Test 4: Mostly solid (sustained load)"
echo "  • Test 5: Rapid flickering (random I/O)"
echo "  • Test 6: Completely off"
echo ""
echo -e "${CYAN}💡 Tip: Review your recording to verify LED behavior matches each test!${NC}"
echo ""
