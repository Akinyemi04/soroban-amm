package gosdk

import (
	"errors"
	"fmt"
	"strings"
	"testing"
)

// allSentinels lists every exported sentinel, contract and client-side, so the
// distinctness check below cannot silently miss a new one.
func allSentinels() map[string]error {
	return map[string]error{
		"ErrAlreadyInitialized":    ErrAlreadyInitialized,
		"ErrInvalidFeeBps":         ErrInvalidFeeBps,
		"ErrInsufficientShares":    ErrInsufficientShares,
		"ErrDeadlineExceeded":      ErrDeadlineExceeded,
		"ErrSlippageExceeded":      ErrSlippageExceeded,
		"ErrPaused":                ErrPaused,
		"ErrUnauthorized":          ErrUnauthorized,
		"ErrZeroAmount":            ErrZeroAmount,
		"ErrInvalidToken":          ErrInvalidToken,
		"ErrEmptyPool":             ErrEmptyPool,
		"ErrInsufficientLiquidity": ErrInsufficientLiquidity,
		"ErrNoPendingAdmin":        ErrNoPendingAdmin,
		"ErrWrongAdmin":            ErrWrongAdmin,
		"ErrReentrant":             ErrReentrant,
		"ErrCircuitBreaker":        ErrCircuitBreaker,
		"ErrInvalidConfig":         ErrInvalidConfig,
		"ErrNoSigner":              ErrNoSigner,
		"ErrDeadlineRequired":      ErrDeadlineRequired,
		"ErrSlippageRequired":      ErrSlippageRequired,
		"ErrTransactionFailed":     ErrTransactionFailed,
		"ErrRPC":                   ErrRPC,
		"ErrXDR":                   ErrXDR,
		"ErrScValue":               ErrScValue,
		"ErrI128OutOfRange":        ErrI128OutOfRange,
		"ErrU128OutOfRange":        ErrU128OutOfRange,
		"ErrSignerAddress":         ErrSignerAddress,
	}
}

func TestSentinelsAreDistinct(t *testing.T) {
	sentinels := allSentinels()
	messages := map[string]string{}
	for name, err := range sentinels {
		if err == nil {
			t.Fatalf("%s is nil", name)
		}
		if prev, ok := messages[err.Error()]; ok {
			t.Fatalf("%s and %s share the message %q", name, prev, err.Error())
		}
		messages[err.Error()] = name

		for other, otherErr := range sentinels {
			if other != name && errors.Is(err, otherErr) {
				t.Fatalf("errors.Is(%s, %s) must be false", name, other)
			}
		}
	}
}

func TestAmmErrorTableMatchesDocumentedDiscriminants(t *testing.T) {
	// docs/error-codes.md numbers AmmError 1..15 with no gaps.
	if len(ammErrorsByCode) != 15 {
		t.Fatalf("ammErrorsByCode has %d entries, want 15", len(ammErrorsByCode))
	}
	seen := map[error]int{}
	for code := 1; code <= 15; code++ {
		sentinel, ok := ammErrorsByCode[code]
		if !ok || sentinel == nil {
			t.Fatalf("code %d has no sentinel", code)
		}
		if prev, dup := seen[sentinel]; dup {
			t.Fatalf("codes %d and %d map to the same sentinel", prev, code)
		}
		seen[sentinel] = code
	}
	// Client-side sentinels must never be reachable from a contract code.
	for _, clientSide := range []error{ErrInvalidConfig, ErrNoSigner, ErrDeadlineRequired, ErrSlippageRequired, ErrTransactionFailed, ErrRPC} {
		if _, ok := seen[clientSide]; ok {
			t.Fatalf("%v is client-side and must not be a contract discriminant", clientSide)
		}
	}
}

func TestDecodeContractErrorParsesRPCShapes(t *testing.T) {
	cases := []struct {
		name     string
		raw      string
		code     int
		sentinel error
	}{
		{"canonical", "HostError: Error(Contract, #5)", 5, ErrSlippageExceeded},
		{"no spaces", "Error(Contract,#6)", 6, ErrPaused},
		{"extra spaces", "Error ( Contract ,  #7 )", 7, ErrUnauthorized},
		{"embedded in diagnostics", "transaction simulation failed: host invocation failed\n\nCaused by:\n    HostError: Error(Contract, #11)\n    DebugInfo not available", 11, ErrInsufficientLiquidity},
		{"first match wins", "Error(Contract, #4) then Error(Contract, #5)", 4, ErrDeadlineExceeded},
		{"leading zeros", "Error(Contract, #0015)", 15, ErrCircuitBreaker},
		{"unmapped code", "Error(Contract, #16)", 16, nil},
		{"code zero", "Error(Contract, #0)", 0, nil},
		{"non-contract host error", "HostError: Error(Budget, ExceededLimit)", 0, nil},
		{"auth error", "HostError: Error(Auth, InvalidAction)", 0, nil},
		{"missing number", "Error(Contract, #)", 0, nil},
		{"negative number", "Error(Contract, #-5)", 0, nil},
		{"lowercase contract", "Error(contract, #5)", 0, nil},
		{"overflowing number", "Error(Contract, #99999999999999999999999)", 0, nil},
		{"empty", "", 0, nil},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			ce := DecodeContractError(tc.raw)
			if ce == nil {
				t.Fatal("DecodeContractError must never return nil")
			}
			if ce.Code != tc.code {
				t.Fatalf("code = %d, want %d", ce.Code, tc.code)
			}
			if ce.Raw != tc.raw {
				t.Fatalf("raw = %q, want the input unchanged", ce.Raw)
			}
			if got := errors.Unwrap(ce); got != tc.sentinel {
				t.Fatalf("unwrapped = %v, want %v", got, tc.sentinel)
			}
			if tc.sentinel != nil && !errors.Is(ce, tc.sentinel) {
				t.Fatalf("errors.Is(%v, %v) = false", ce, tc.sentinel)
			}
		})
	}
}

func TestDecodeContractErrorMatchesOnlyItsOwnSentinel(t *testing.T) {
	ce := DecodeContractError("Error(Contract, #5)")
	for name, sentinel := range allSentinels() {
		if sentinel == ErrSlippageExceeded {
			continue
		}
		if errors.Is(ce, sentinel) {
			t.Fatalf("code 5 must not match %s", name)
		}
	}
}

func TestContractErrorMessage(t *testing.T) {
	mapped := DecodeContractError("HostError: Error(Contract, #3)")
	if got, want := mapped.Error(), "contract error 3: insufficient shares"; got != want {
		t.Fatalf("Error() = %q, want %q", got, want)
	}

	unmapped := DecodeContractError("HostError: Error(Contract, #42)")
	if got := unmapped.Error(); !strings.Contains(got, "Error(Contract, #42)") {
		t.Fatalf("an unmapped code should keep the raw text, got %q", got)
	}

	unparsed := DecodeContractError("connection reset")
	if got, want := unparsed.Error(), "contract error: connection reset"; got != want {
		t.Fatalf("Error() = %q, want %q", got, want)
	}
}

func TestContractErrorSurvivesWrapping(t *testing.T) {
	// Callers commonly add context with %w; both errors.Is and errors.As must
	// still see through it.
	inner := DecodeContractError("Error(Contract, #8)")
	wrapped := fmt.Errorf("swap on pool X: %w", fmt.Errorf("attempt 2: %w", inner))

	if !errors.Is(wrapped, ErrZeroAmount) {
		t.Fatalf("errors.Is through two wraps failed: %v", wrapped)
	}
	var ce *ContractError
	if !errors.As(wrapped, &ce) {
		t.Fatalf("errors.As through two wraps failed: %v", wrapped)
	}
	if ce != inner || ce.Code != 8 {
		t.Fatalf("errors.As found %+v, want the original *ContractError", ce)
	}
}

func TestClientErrorsAreNotContractErrors(t *testing.T) {
	for _, err := range []error{
		fmt.Errorf("%w: http 502", ErrRPC),
		fmt.Errorf("%w: rejected", ErrTransactionFailed),
		ErrNoSigner,
		fmt.Errorf("%w: RPCURL is required", ErrInvalidConfig),
	} {
		var ce *ContractError
		if errors.As(err, &ce) {
			t.Fatalf("%v must not be classified as a contract error", err)
		}
	}
}

func TestZeroValueContractError(t *testing.T) {
	// A hand-built ContractError has no sentinel; it must still format and
	// unwrap safely.
	var ce ContractError
	if ce.Unwrap() != nil {
		t.Fatal("zero-value ContractError must unwrap to nil")
	}
	if ce.Error() != "contract error: " {
		t.Fatalf("Error() = %q", ce.Error())
	}
}
