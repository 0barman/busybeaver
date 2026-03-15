#!/bin/bash
#
# check.sh - Code quality check script for Beaver library
#
# This script performs the following checks:
# 1. Format code with cargo fmt
# 2. Run all tests
# 3. Check for dangerous code patterns (panic!, unwrap, expect, etc.)
# 4. Check for compiler warnings
#

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Get script directory and project root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

cd "$PROJECT_ROOT"

echo -e "${BLUE}========================================${NC}"
echo -e "${BLUE}  Beaver Library - Code Quality Check  ${NC}"
echo -e "${BLUE}========================================${NC}"
echo ""

# Track overall status
OVERALL_STATUS=0

# =============================================================================
# Step 1: Format code with cargo fmt
# =============================================================================
echo -e "${YELLOW}[1/4] Running cargo fmt...${NC}"
if cargo fmt --all -- --check 2>/dev/null; then
    echo -e "${GREEN}✓ Code is properly formatted${NC}"
else
    echo -e "${YELLOW}! Code needs formatting, applying cargo fmt...${NC}"
    cargo fmt --all
    echo -e "${GREEN}✓ Code formatted successfully${NC}"
fi
echo ""

# =============================================================================
# Step 2: Run all tests
# =============================================================================
echo -e "${YELLOW}[2/4] Running all tests...${NC}"
if cargo test 2>&1 | tee /tmp/test_output.txt; then
    # Sum up all passed tests from all test results
    TOTAL_PASSED=$(grep -E "^test result:" /tmp/test_output.txt | grep -oE "[0-9]+ passed" | awk '{sum += $1} END {print sum}')
    echo -e "${GREEN}✓ All tests passed (${TOTAL_PASSED:-0} total)${NC}"
else
    echo -e "${RED}✗ Some tests failed${NC}"
    OVERALL_STATUS=1
fi
echo ""

# =============================================================================
# Step 3: Check for compiler warnings
# =============================================================================
echo -e "${YELLOW}[3/4] Checking for compiler warnings...${NC}"
WARNINGS=$(cargo build 2>&1 | grep -c "warning:" || true)
if [ "$WARNINGS" -gt 0 ]; then
    echo -e "${YELLOW}! Found $WARNINGS warning(s):${NC}"
    cargo build 2>&1 | grep "warning:" | head -20
    echo ""
    echo -e "${YELLOW}  (Run 'cargo build' to see full details)${NC}"
else
    echo -e "${GREEN}✓ No compiler warnings${NC}"
fi
echo ""

# =============================================================================
# Step 4: Check for dangerous code patterns
# =============================================================================
echo -e "${YELLOW}[4/4] Checking for dangerous code patterns...${NC}"

DANGEROUS_FOUND=0

check_pattern() {
    local pattern="$1"
    local description="$2"
    local exclude_tests="$3"
    
    if [ "$exclude_tests" = "true" ]; then
        # Exclude tests directory, also exclude comments (lines starting with // or ///)
        MATCHES=$(grep -rn "$pattern" --include="*.rs" src/ 2>/dev/null | grep -v '^\s*///' | grep -v '^\s*//' | grep -v ':.*//.*'"$pattern" || true)
    else
        # Include all Rust files
        MATCHES=$(grep -rn "$pattern" --include="*.rs" src/ tests/ 2>/dev/null | grep -v '^\s*///' | grep -v '^\s*//' || true)
    fi
    
    # Filter out doc comments and inline comments more thoroughly
    if [ -n "$MATCHES" ]; then
        # Further filter: exclude lines where pattern appears after // (inline comment)
        FILTERED_MATCHES=""
        while IFS= read -r line; do
            # Extract the code part (file:line:content)
            content=$(echo "$line" | cut -d: -f3-)
            # Check if the pattern is in a comment
            if ! echo "$content" | grep -qE '^\s*//' && ! echo "$content" | grep -qE '//.*'"$pattern"; then
                FILTERED_MATCHES="${FILTERED_MATCHES}${line}"$'\n'
            fi
        done <<< "$MATCHES"
        MATCHES=$(echo "$FILTERED_MATCHES" | grep -v '^$' || true)
    fi
    
    if [ -n "$MATCHES" ]; then
        COUNT=$(echo "$MATCHES" | wc -l | tr -d ' ')
        echo -e "${YELLOW}  $description: $COUNT occurrence(s)${NC}"
        echo "$MATCHES" | while read -r line; do
            echo -e "    ${RED}→${NC} $line"
        done
        return 1
    fi
    return 0
}

echo ""
echo "Checking src/ directory (production code):"
echo "-------------------------------------------"

# Check panic! in src/
if ! check_pattern 'panic!' 'panic! macro' "true"; then
    DANGEROUS_FOUND=1
fi

# Check unwrap() in src/
if ! check_pattern '\.unwrap()' '.unwrap() calls' "true"; then
    DANGEROUS_FOUND=1
fi

# Check expect() in src/
if ! check_pattern '\.expect(' '.expect() calls' "true"; then
    DANGEROUS_FOUND=1
fi

# Check todo! in src/
if ! check_pattern 'todo!' 'todo! macro' "true"; then
    DANGEROUS_FOUND=1
fi

# Check unimplemented! in src/
if ! check_pattern 'unimplemented!' 'unimplemented! macro' "true"; then
    DANGEROUS_FOUND=1
fi

# Check unreachable! in src/ (may be intentional, just warn)
if ! check_pattern 'unreachable!' 'unreachable! macro' "true"; then
    echo -e "${YELLOW}    (Note: unreachable! may be intentional)${NC}"
fi

# Check unsafe blocks in src/
if ! check_pattern 'unsafe' 'unsafe blocks' "true"; then
    echo -e "${YELLOW}    (Note: unsafe may be necessary, review carefully)${NC}"
fi

echo ""
echo "Checking tests/ directory (test code):"
echo "---------------------------------------"

# In tests, unwrap/expect are acceptable but we still report them
TESTS_UNWRAP=$(grep -rn '\.unwrap()' --include="*.rs" tests/ 2>/dev/null | wc -l | tr -d ' ')
TESTS_EXPECT=$(grep -rn '\.expect(' --include="*.rs" tests/ 2>/dev/null | wc -l | tr -d ' ')
TESTS_PANIC=$(grep -rn 'panic!' --include="*.rs" tests/ 2>/dev/null | wc -l | tr -d ' ')

echo -e "  ${BLUE}.unwrap() calls: $TESTS_UNWRAP (acceptable in tests)${NC}"
echo -e "  ${BLUE}.expect() calls: $TESTS_EXPECT (acceptable in tests)${NC}"
echo -e "  ${BLUE}panic! macros: $TESTS_PANIC (acceptable in tests)${NC}"

echo ""

if [ "$DANGEROUS_FOUND" -eq 1 ]; then
    echo -e "${YELLOW}⚠ Found potentially dangerous patterns in production code${NC}"
    echo -e "${YELLOW}  Please review and handle errors properly instead of panicking${NC}"
else
    echo -e "${GREEN}✓ No dangerous patterns found in production code${NC}"
fi

echo ""
echo -e "${BLUE}========================================${NC}"
echo -e "${BLUE}           Summary                     ${NC}"
echo -e "${BLUE}========================================${NC}"

if [ "$OVERALL_STATUS" -eq 0 ] && [ "$DANGEROUS_FOUND" -eq 0 ]; then
    echo -e "${GREEN}✓ All checks passed!${NC}"
    exit 0
else
    echo -e "${YELLOW}⚠ Some issues were found, please review above${NC}"
    exit 1
fi
