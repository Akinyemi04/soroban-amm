package gosdk

import (
	"bytes"
	"context"
	"errors"
	"math/big"
	"strings"
	"testing"
	"time"
)

// ── Constants ─────────────────────────────────────────────────────────────────

func TestNetworkPassphrasesAreExact(t *testing.T) {
	// Passphrases are hashed into every signature payload, so a single wrong
	// character produces transactions the network rejects. These are the
	// values published by the Stellar Development Foundation.
	cases := map[string]struct{ got, want string }{
		"testnet":   {NetworkTestnet, "Test SDF Network ; September 2015"},
		"public":    {NetworkPublic, "Public Global Stellar Network ; September 2015"},
		"futurenet": {NetworkFuturenet, "Test SDF Future Network ; October 2022"},
	}
	for name, tc := range cases {
		if tc.got != tc.want {
			t.Fatalf("%s passphrase = %q, want %q", name, tc.got, tc.want)
		}
	}
}

func TestDefaultSimulationAccountIsTheNullAccount(t *testing.T) {
	raw, kind, err := DecodeStrkey(DefaultSimulationAccount)
	if err != nil {
		t.Fatalf("DecodeStrkey: %v", err)
	}
	if kind != StrkeyAccount {
		t.Fatalf("kind = %v, want StrkeyAccount", kind)
	}
	if !bytes.Equal(raw, make([]byte, 32)) {
		t.Fatalf("payload = %x, want 32 zero bytes", raw)
	}
	if !IsAccountAddress(DefaultSimulationAccount) {
		t.Fatal("DefaultSimulationAccount must pass IsAccountAddress")
	}
}

func TestConfigTimeoutIsApplied(t *testing.T) {
	c, err := NewClient(Config{RPCURL: "http://localhost:8000", NetworkPassphrase: NetworkTestnet, Timeout: 3 * time.Second})
	if err != nil {
		t.Fatalf("NewClient: %v", err)
	}
	if c.http.Timeout != 3*time.Second {
		t.Fatalf("timeout = %v, want 3s", c.http.Timeout)
	}
}

func TestConfigSourceAccountIsUsed(t *testing.T) {
	custom, err := EncodeStrkey(bytes.Repeat([]byte{7}, 32), StrkeyAccount)
	if err != nil {
		t.Fatal(err)
	}
	c, err := NewClient(Config{RPCURL: "http://localhost:8000", NetworkPassphrase: NetworkTestnet, SourceAccount: custom})
	if err != nil {
		t.Fatalf("NewClient: %v", err)
	}
	if c.sourceAccount != custom {
		t.Fatalf("source = %q, want %q", c.sourceAccount, custom)
	}

	// A contract address cannot source a transaction.
	if _, err := NewClient(Config{RPCURL: "http://localhost:8000", NetworkPassphrase: NetworkTestnet, SourceAccount: zeroContract}); !errors.Is(err, ErrInvalidConfig) {
		t.Fatalf("expected ErrInvalidConfig for a contract source, got %v", err)
	}
}

// ── ScValue → PoolInfo / SwapQuote ────────────────────────────────────────────

// distinctPoolInfo gives every field a different value, so a field read from
// the wrong key cannot pass by coincidence.
func distinctPoolInfo(t *testing.T) (ScValue, map[string]string) {
	t.Helper()
	addr := func(b byte, kind StrkeyKind) string {
		s, err := EncodeStrkey(bytes.Repeat([]byte{b}, 32), kind)
		if err != nil {
			t.Fatal(err)
		}
		return s
	}
	tokenA, tokenB := addr(1, StrkeyContract), addr(2, StrkeyContract)
	admin, recipient := addr(3, StrkeyAccount), addr(4, StrkeyAccount)

	v := Map(
		ScMapEntry{Key: Symbol("admin"), Val: Addr(admin)},
		ScMapEntry{Key: Symbol("fee_bps"), Val: I128(big.NewInt(30))},
		ScMapEntry{Key: Symbol("fee_recipient"), Val: Addr(recipient)},
		ScMapEntry{Key: Symbol("flash_loan_fee_bps"), Val: I128(big.NewInt(9))},
		ScMapEntry{Key: Symbol("lp_rebate_bps"), Val: I128(big.NewInt(2500))},
		ScMapEntry{Key: Symbol("protocol_fee_bps"), Val: I128(big.NewInt(1600))},
		ScMapEntry{Key: Symbol("reserve_a"), Val: I128(big.NewInt(111))},
		ScMapEntry{Key: Symbol("reserve_b"), Val: I128(big.NewInt(222))},
		ScMapEntry{Key: Symbol("token_a"), Val: Addr(tokenA)},
		ScMapEntry{Key: Symbol("token_b"), Val: Addr(tokenB)},
		ScMapEntry{Key: Symbol("total_shares"), Val: I128(big.NewInt(333))},
	)
	return v, map[string]string{"token_a": tokenA, "token_b": tokenB, "admin": admin, "fee_recipient": recipient}
}

func TestParsePoolInfoMapsEveryField(t *testing.T) {
	v, addrs := distinctPoolInfo(t)
	info, err := parsePoolInfo(v)
	if err != nil {
		t.Fatalf("parsePoolInfo: %v", err)
	}

	for name, got := range map[string]string{
		"token_a": info.TokenA, "token_b": info.TokenB,
		"admin": info.Admin, "fee_recipient": info.FeeRecipient,
	} {
		if got != addrs[name] {
			t.Fatalf("%s = %q, want %q", name, got, addrs[name])
		}
	}
	for name, tc := range map[string]struct {
		got  *big.Int
		want int64
	}{
		"reserve_a":          {info.ReserveA, 111},
		"reserve_b":          {info.ReserveB, 222},
		"total_shares":       {info.TotalShares, 333},
		"fee_bps":            {info.FeeBps, 30},
		"flash_loan_fee_bps": {info.FlashLoanFeeBps, 9},
		"protocol_fee_bps":   {info.ProtocolFeeBps, 1600},
		"lp_rebate_bps":      {info.LpRebateBps, 2500},
	} {
		if tc.got == nil || tc.got.Int64() != tc.want {
			t.Fatalf("%s = %v, want %d", name, tc.got, tc.want)
		}
	}
}

func TestParsePoolInfoIgnoresFieldOrderAndExtras(t *testing.T) {
	v, _ := distinctPoolInfo(t)
	reversed := make([]ScMapEntry, 0, len(v.Map)+1)
	reversed = append(reversed, ScMapEntry{Key: Symbol("future_field"), Val: Str("ignored")})
	for i := len(v.Map) - 1; i >= 0; i-- {
		reversed = append(reversed, v.Map[i])
	}
	info, err := parsePoolInfo(Map(reversed...))
	if err != nil {
		t.Fatalf("parsePoolInfo: %v", err)
	}
	if info.ReserveB.Int64() != 222 {
		t.Fatalf("reserve_b = %s, want 222", info.ReserveB)
	}
}

func TestParsePoolInfoRejectsEachMissingField(t *testing.T) {
	v, _ := distinctPoolInfo(t)
	for i, e := range v.Map {
		name := e.Key.Str
		t.Run(name, func(t *testing.T) {
			without := append(append([]ScMapEntry{}, v.Map[:i]...), v.Map[i+1:]...)
			_, err := parsePoolInfo(Map(without...))
			if !errors.Is(err, ErrScValue) {
				t.Fatalf("expected ErrScValue, got %v", err)
			}
			if !strings.Contains(err.Error(), name) {
				t.Fatalf("error %q should name the missing field %q", err.Error(), name)
			}
		})
	}
}

func TestParsePoolInfoRejectsWrongFieldTypes(t *testing.T) {
	cases := map[string]ScValue{
		"token_a":       I128(big.NewInt(1)),      // integer where an address belongs
		"admin":         Str(zeroAccount),         // strkey text is not an address value
		"reserve_a":     Addr(zeroContract),       // address where an integer belongs
		"total_shares":  Str("1000"),              // numeric text is not an integer
		"fee_bps":       Bool(true),               // bool is not an integer
		"reserve_b":     ScValue{Type: scvI128},   // 128-bit value with no payload
		"lp_rebate_bps": Vec(I128(big.NewInt(1))), // a vec is not an integer
	}
	for field, bad := range cases {
		t.Run(field, func(t *testing.T) {
			v, _ := distinctPoolInfo(t)
			for i := range v.Map {
				if v.Map[i].Key.Str == field {
					v.Map[i].Val = bad
				}
			}
			if _, err := parsePoolInfo(v); !errors.Is(err, ErrScValue) {
				t.Fatalf("expected ErrScValue, got %v", err)
			}
		})
	}
}

func TestParsePoolInfoRejectsNonMap(t *testing.T) {
	for name, v := range map[string]ScValue{
		"void":  Void(),
		"vec":   Vec(I128(big.NewInt(1)), I128(big.NewInt(2))),
		"i128":  I128(big.NewInt(1)),
		"empty": Map(),
	} {
		if _, err := parsePoolInfo(v); !errors.Is(err, ErrScValue) {
			t.Fatalf("%s: expected ErrScValue, got %v", name, err)
		}
	}
}

func TestParsePoolInfoKeysMustBeSymbols(t *testing.T) {
	// A string key that happens to spell a field name is not the field.
	v, _ := distinctPoolInfo(t)
	for i := range v.Map {
		if v.Map[i].Key.Str == "reserve_a" {
			v.Map[i].Key = Str("reserve_a")
		}
	}
	if _, err := parsePoolInfo(v); !errors.Is(err, ErrScValue) {
		t.Fatalf("expected ErrScValue for a string-keyed field, got %v", err)
	}
}

func TestParsePoolInfoIsLosslessAtI128Extremes(t *testing.T) {
	v, _ := distinctPoolInfo(t)
	for i := range v.Map {
		switch v.Map[i].Key.Str {
		case "reserve_a":
			v.Map[i].Val = I128(new(big.Int).Set(MaxI128))
		case "reserve_b":
			v.Map[i].Val = I128(new(big.Int).Set(MinI128))
		case "total_shares":
			v.Map[i].Val = U128(new(big.Int).Set(MaxU128))
		}
	}

	// Go through the wire so the extremes survive encode, decode and parse.
	encoded, err := EncodeScValBase64(v)
	if err != nil {
		t.Fatalf("encode: %v", err)
	}
	decoded, err := DecodeScValBase64(encoded)
	if err != nil {
		t.Fatalf("decode: %v", err)
	}
	info, err := parsePoolInfo(decoded)
	if err != nil {
		t.Fatalf("parsePoolInfo: %v", err)
	}
	if info.ReserveA.Cmp(MaxI128) != 0 {
		t.Fatalf("reserve_a = %s, want MaxI128", info.ReserveA)
	}
	if info.ReserveB.Cmp(MinI128) != 0 {
		t.Fatalf("reserve_b = %s, want MinI128", info.ReserveB)
	}
	if info.TotalShares.Cmp(MaxU128) != 0 {
		t.Fatalf("total_shares = %s, want MaxU128", info.TotalShares)
	}
}

func TestParsePoolInfoAcceptsNarrowIntegers(t *testing.T) {
	// A contract that narrows a bps field to u32 must still decode.
	v, _ := distinctPoolInfo(t)
	for i := range v.Map {
		if v.Map[i].Key.Str == "fee_bps" {
			v.Map[i].Val = U32(30)
		}
	}
	info, err := parsePoolInfo(v)
	if err != nil {
		t.Fatalf("parsePoolInfo: %v", err)
	}
	if info.FeeBps.Int64() != 30 {
		t.Fatalf("fee_bps = %s, want 30", info.FeeBps)
	}
}

func TestParsedIntegersDoNotAliasTheScValue(t *testing.T) {
	reserve := big.NewInt(111)
	v, _ := distinctPoolInfo(t)
	for i := range v.Map {
		if v.Map[i].Key.Str == "reserve_a" {
			v.Map[i].Val = I128(reserve)
		}
	}
	info, err := parsePoolInfo(v)
	if err != nil {
		t.Fatalf("parsePoolInfo: %v", err)
	}
	info.ReserveA.SetInt64(999)
	if reserve.Int64() != 111 {
		t.Fatalf("mutating PoolInfo.ReserveA changed the source value to %s", reserve)
	}
}

func TestParseSwapQuoteMapsEveryField(t *testing.T) {
	v := Map(
		ScMapEntry{Key: Symbol("amount_out"), Val: I128(big.NewInt(1))},
		ScMapEntry{Key: Symbol("effective_price"), Val: I128(big.NewInt(2))},
		ScMapEntry{Key: Symbol("fee_amount"), Val: I128(big.NewInt(3))},
		ScMapEntry{Key: Symbol("price_impact_bps"), Val: I128(big.NewInt(4))},
		ScMapEntry{Key: Symbol("spot_price"), Val: I128(big.NewInt(5))},
	)
	q, err := parseSwapQuote(v)
	if err != nil {
		t.Fatalf("parseSwapQuote: %v", err)
	}
	got := []int64{q.AmountOut.Int64(), q.EffectivePrice.Int64(), q.FeeAmount.Int64(), q.PriceImpactBps.Int64(), q.SpotPrice.Int64()}
	for i, want := range []int64{1, 2, 3, 4, 5} {
		if got[i] != want {
			t.Fatalf("field %d = %d, want %d (fields: %v)", i, got[i], want, got)
		}
	}

	for i, e := range v.Map {
		name := e.Key.Str
		without := append(append([]ScMapEntry{}, v.Map[:i]...), v.Map[i+1:]...)
		if _, err := parseSwapQuote(Map(without...)); !errors.Is(err, ErrScValue) || !strings.Contains(err.Error(), name) {
			t.Fatalf("missing %s: expected an ErrScValue naming it, got %v", name, err)
		}
	}
}

func TestParseSwapQuoteKeepsNegativeValues(t *testing.T) {
	// price_impact_bps is signed on-chain; the sign must survive conversion.
	q, err := parseSwapQuote(Map(
		ScMapEntry{Key: Symbol("amount_out"), Val: I128(big.NewInt(0))},
		ScMapEntry{Key: Symbol("effective_price"), Val: I128(big.NewInt(0))},
		ScMapEntry{Key: Symbol("fee_amount"), Val: I128(big.NewInt(0))},
		ScMapEntry{Key: Symbol("price_impact_bps"), Val: I128(big.NewInt(-12))},
		ScMapEntry{Key: Symbol("spot_price"), Val: I128(big.NewInt(0))},
	))
	if err != nil {
		t.Fatalf("parseSwapQuote: %v", err)
	}
	if q.PriceImpactBps.Int64() != -12 {
		t.Fatalf("price_impact_bps = %s, want -12", q.PriceImpactBps)
	}
}

// ── ScValue.BigInt: the integer conversion every struct field goes through ────

func TestBigIntConversionIsLossless(t *testing.T) {
	cases := []struct {
		name string
		val  ScValue
		want string
	}{
		{"u32 max", U32(^uint32(0)), "4294967295"},
		{"i32 min", I32(-2_147_483_648), "-2147483648"},
		{"u64 max stays positive", U64(^uint64(0)), "18446744073709551615"},
		{"i64 min", I64(-9_223_372_036_854_775_808), "-9223372036854775808"},
		{"i128 max", I128(new(big.Int).Set(MaxI128)), MaxI128.String()},
		{"i128 min", I128(new(big.Int).Set(MinI128)), MinI128.String()},
		{"u128 max", U128(new(big.Int).Set(MaxU128)), MaxU128.String()},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got, err := tc.val.BigInt()
			if err != nil {
				t.Fatalf("BigInt: %v", err)
			}
			if got.String() != tc.want {
				t.Fatalf("BigInt = %s, want %s", got, tc.want)
			}
		})
	}
}

func TestBigIntRejectsNonIntegers(t *testing.T) {
	for name, v := range map[string]ScValue{
		"bool":     Bool(true),
		"void":     Void(),
		"bytes":    Bytes([]byte{1}),
		"string":   Str("1"),
		"symbol":   Symbol("one"),
		"address":  Addr(zeroAccount),
		"vec":      Vec(U32(1)),
		"map":      Map(),
		"nil i128": ScValue{Type: scvI128},
		"nil u128": ScValue{Type: scvU128},
	} {
		if _, err := v.BigInt(); !errors.Is(err, ErrScValue) {
			t.Fatalf("%s: expected ErrScValue, got %v", name, err)
		}
	}
}

func TestAddressConversionRejectsNonAddresses(t *testing.T) {
	for name, v := range map[string]ScValue{
		"string strkey": Str(zeroAccount),
		"symbol":        Symbol("admin"),
		"i128":          I128(big.NewInt(1)),
	} {
		if _, err := v.Address(); !errors.Is(err, ErrScValue) {
			t.Fatalf("%s: expected ErrScValue, got %v", name, err)
		}
	}
}

// ── Reserves / TxResult via the client ────────────────────────────────────────

func TestGetReservesCopiesFromPoolInfo(t *testing.T) {
	v, _ := distinctPoolInfo(t)
	rec := newRecordedRPC(t).respond("simulateTransaction", simulationOf(t, v))
	c := newTestClient(t, rec, nil)

	r, err := c.GetReserves(context.Background(), zeroContract)
	if err != nil {
		t.Fatalf("GetReserves: %v", err)
	}
	if r.ReserveA.Int64() != 111 || r.ReserveB.Int64() != 222 {
		t.Fatalf("reserves = (%s, %s), want (111, 222)", r.ReserveA, r.ReserveB)
	}
}

func TestGetInfoRejectsMalformedReturnValue(t *testing.T) {
	cases := map[string]map[string]interface{}{
		"not base64":        {"results": []map[string]string{{"xdr": "!!!not base64!!!"}}},
		"not xdr":           {"results": []map[string]string{{"xdr": "AAAA/w=="}}},
		"wrong return type": simulationOf(t, I128(big.NewInt(1))),
	}
	for name, result := range cases {
		t.Run(name, func(t *testing.T) {
			rec := newRecordedRPC(t).respond("simulateTransaction", result)
			c := newTestClient(t, rec, nil)

			info, err := c.GetInfo(context.Background(), zeroContract)
			if err == nil {
				t.Fatalf("expected an error, got %+v", info)
			}
			if !errors.Is(err, ErrScValue) && !errors.Is(err, ErrXDR) {
				t.Fatalf("expected ErrScValue or ErrXDR, got %v", err)
			}
		})
	}
}
