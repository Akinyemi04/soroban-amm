package gosdk

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"math/big"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

// rawServer serves one fixed status and body regardless of the request.
func rawServer(t *testing.T, status int, body string) *Client {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(status)
		_, _ = io.WriteString(w, body)
	}))
	t.Cleanup(srv.Close)
	return clientFor(t, srv.URL)
}

func clientFor(t *testing.T, url string) *Client {
	t.Helper()
	c, err := NewClient(Config{RPCURL: url, NetworkPassphrase: NetworkTestnet})
	if err != nil {
		t.Fatalf("NewClient: %v", err)
	}
	return c.WithPolling(time.Millisecond, 5)
}

// ── call: the JSON-RPC transport ──────────────────────────────────────────────

func TestCallSendsWellFormedJSONRPC(t *testing.T) {
	var (
		gotMethod, gotContentType string
		gotBody                   map[string]json.RawMessage
	)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotMethod = r.Method
		gotContentType = r.Header.Get("Content-Type")
		if err := json.NewDecoder(r.Body).Decode(&gotBody); err != nil {
			t.Errorf("decoding request: %v", err)
		}
		_, _ = io.WriteString(w, `{"jsonrpc":"2.0","id":1,"result":{"status":"SUCCESS"}}`)
	}))
	t.Cleanup(srv.Close)
	c := clientFor(t, srv.URL)

	var out GetTxResult
	if err := c.call(context.Background(), "getTransaction", map[string]string{"hash": "abc"}, &out); err != nil {
		t.Fatalf("call: %v", err)
	}
	if out.Status != "SUCCESS" {
		t.Fatalf("status = %q, want SUCCESS", out.Status)
	}
	if gotMethod != http.MethodPost {
		t.Fatalf("http method = %s, want POST", gotMethod)
	}
	if gotContentType != "application/json" {
		t.Fatalf("content type = %q", gotContentType)
	}
	for key, want := range map[string]string{
		"jsonrpc": `"2.0"`,
		"id":      `1`,
		"method":  `"getTransaction"`,
		"params":  `{"hash":"abc"}`,
	} {
		if got := string(gotBody[key]); got != want {
			t.Fatalf("request %s = %s, want %s", key, got, want)
		}
	}
}

func TestCallOmitsNilParams(t *testing.T) {
	var body map[string]json.RawMessage
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewDecoder(r.Body).Decode(&body)
		_, _ = io.WriteString(w, `{"jsonrpc":"2.0","id":1,"result":{}}`)
	}))
	t.Cleanup(srv.Close)

	if err := clientFor(t, srv.URL).call(context.Background(), "getHealth", nil, nil); err != nil {
		t.Fatalf("call: %v", err)
	}
	if _, ok := body["params"]; ok {
		t.Fatalf("nil params must be omitted, got %s", body["params"])
	}
}

func TestCallResponses(t *testing.T) {
	cases := []struct {
		name    string
		status  int
		body    string
		wantErr bool
		// contains is checked against the error text when wantErr is set.
		contains []string
	}{
		{
			name:   "success",
			status: http.StatusOK,
			body:   `{"jsonrpc":"2.0","id":1,"result":{"status":"PENDING","hash":"h1"}}`,
		},
		{
			name:   "success with unknown fields",
			status: http.StatusOK,
			body:   `{"jsonrpc":"2.0","id":1,"result":{"status":"PENDING","hash":"h1","latestLedger":5},"extra":true}`,
		},
		{
			name:     "rpc error object",
			status:   http.StatusOK,
			body:     `{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"invalid params"}}`,
			wantErr:  true,
			contains: []string{"sendTransaction", "-32602", "invalid params"},
		},
		{
			name:     "rpc error wins over result",
			status:   http.StatusOK,
			body:     `{"jsonrpc":"2.0","id":1,"result":{"status":"PENDING"},"error":{"code":-32000,"message":"boom"}}`,
			wantErr:  true,
			contains: []string{"-32000", "boom"},
		},
		{
			name:     "malformed json",
			status:   http.StatusOK,
			body:     `{"jsonrpc":"2.0","id":1,"result":`,
			wantErr:  true,
			contains: []string{"decoding sendTransaction response"},
		},
		{
			name:     "not json at all",
			status:   http.StatusOK,
			body:     `<html>gateway</html>`,
			wantErr:  true,
			contains: []string{"decoding sendTransaction response"},
		},
		{
			name:     "empty body",
			status:   http.StatusOK,
			body:     ``,
			wantErr:  true,
			contains: []string{"decoding sendTransaction response"},
		},
		{
			name:     "neither result nor error",
			status:   http.StatusOK,
			body:     `{"jsonrpc":"2.0","id":1}`,
			wantErr:  true,
			contains: []string{"decoding sendTransaction result"},
		},
		{
			name:     "result of the wrong shape",
			status:   http.StatusOK,
			body:     `{"jsonrpc":"2.0","id":1,"result":"PENDING"}`,
			wantErr:  true,
			contains: []string{"decoding sendTransaction result"},
		},
		{
			name:     "result field of the wrong type",
			status:   http.StatusOK,
			body:     `{"jsonrpc":"2.0","id":1,"result":{"status":42}}`,
			wantErr:  true,
			contains: []string{"decoding sendTransaction result"},
		},
		{
			name:     "no content",
			status:   http.StatusNoContent,
			body:     ``,
			wantErr:  true,
			contains: []string{"decoding sendTransaction response"},
		},
		{
			name:     "bad gateway",
			status:   http.StatusBadGateway,
			body:     `upstream exploded`,
			wantErr:  true,
			contains: []string{"http 502", "upstream exploded"},
		},
		{
			name:     "not found",
			status:   http.StatusNotFound,
			body:     `no such route`,
			wantErr:  true,
			contains: []string{"http 404"},
		},
		{
			name:     "rate limited",
			status:   http.StatusTooManyRequests,
			body:     `slow down`,
			wantErr:  true,
			contains: []string{"http 429", "slow down"},
		},
		{
			name:     "server error with a valid json-rpc body",
			status:   http.StatusInternalServerError,
			body:     `{"jsonrpc":"2.0","id":1,"result":{"status":"PENDING"}}`,
			wantErr:  true,
			contains: []string{"http 500"},
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			c := rawServer(t, tc.status, tc.body)

			var out SendResult
			err := c.call(context.Background(), "sendTransaction", map[string]string{"transaction": "AAAA"}, &out)
			if !tc.wantErr {
				if err != nil {
					t.Fatalf("call: %v", err)
				}
				if out.Status != "PENDING" || out.Hash != "h1" {
					t.Fatalf("decoded %+v", out)
				}
				return
			}
			if !errors.Is(err, ErrRPC) {
				t.Fatalf("expected ErrRPC, got %v", err)
			}
			var ce *ContractError
			if errors.As(err, &ce) {
				t.Fatalf("a transport failure must not look like a contract error: %v", err)
			}
			for _, want := range tc.contains {
				if !strings.Contains(err.Error(), want) {
					t.Fatalf("error %q should mention %q", err.Error(), want)
				}
			}
		})
	}
}

func TestCallTruncatesLongHTTPErrorBodies(t *testing.T) {
	c := rawServer(t, http.StatusServiceUnavailable, strings.Repeat("x", 10_000))

	err := c.call(context.Background(), "getHealth", nil, nil)
	if !errors.Is(err, ErrRPC) {
		t.Fatalf("expected ErrRPC, got %v", err)
	}
	if len(err.Error()) > 400 {
		t.Fatalf("error message is %d bytes; the body should be truncated", len(err.Error()))
	}
	if !strings.HasSuffix(err.Error(), "...") {
		t.Fatalf("truncated body should end with an ellipsis, got %q", err.Error())
	}
}

func TestCallCapsResponseSize(t *testing.T) {
	// A response larger than maxResponseBytes is cut off, so its JSON no longer
	// parses; the client must fail rather than buffer it all.
	huge := `{"jsonrpc":"2.0","id":1,"result":{"hash":"` + strings.Repeat("a", maxResponseBytes) + `"}}`
	c := rawServer(t, http.StatusOK, huge)

	var out SendResult
	err := c.call(context.Background(), "sendTransaction", nil, &out)
	if !errors.Is(err, ErrRPC) {
		t.Fatalf("expected ErrRPC for an oversized response, got %v", err)
	}
}

func TestCallRejectsUnencodableParams(t *testing.T) {
	c := clientFor(t, "http://127.0.0.1:1")
	err := c.call(context.Background(), "getHealth", map[string]interface{}{"bad": make(chan int)}, nil)
	if !errors.Is(err, ErrRPC) || !strings.Contains(err.Error(), "encoding getHealth request") {
		t.Fatalf("expected an encoding ErrRPC, got %v", err)
	}
}

func TestCallRejectsUnbuildableRequest(t *testing.T) {
	c := clientFor(t, "http://bad\x7fhost")
	err := c.call(context.Background(), "getHealth", nil, nil)
	if !errors.Is(err, ErrRPC) || !strings.Contains(err.Error(), "building getHealth request") {
		t.Fatalf("expected a building ErrRPC, got %v", err)
	}
}

func TestCallSurfacesConnectionFailure(t *testing.T) {
	srv := httptest.NewServer(http.NotFoundHandler())
	url := srv.URL
	srv.Close() // nothing is listening any more

	err := clientFor(t, url).call(context.Background(), "getHealth", nil, nil)
	if !errors.Is(err, ErrRPC) {
		t.Fatalf("expected ErrRPC, got %v", err)
	}
	var opErr *net.OpError
	if !errors.As(err, &opErr) {
		t.Fatalf("the underlying network error should be reachable with errors.As, got %v", err)
	}
}

// blockingServer holds every request open until the client gives up or the
// test ends.
func blockingServer(t *testing.T) string {
	t.Helper()
	release := make(chan struct{})
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		select {
		case <-r.Context().Done():
		case <-release:
		}
	}))
	t.Cleanup(func() {
		close(release)
		srv.Close()
	})
	return srv.URL
}

func TestCallHonoursContextCancellation(t *testing.T) {
	c := clientFor(t, blockingServer(t))

	ctx, cancel := context.WithCancel(context.Background())
	go func() {
		time.Sleep(20 * time.Millisecond)
		cancel()
	}()

	start := time.Now()
	err := c.call(ctx, "getHealth", nil, nil)
	if !errors.Is(err, ErrRPC) {
		t.Fatalf("expected ErrRPC, got %v", err)
	}
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("expected context.Canceled to be reachable, got %v", err)
	}
	if elapsed := time.Since(start); elapsed > 5*time.Second {
		t.Fatalf("call took %v; cancellation was not honoured", elapsed)
	}
}

func TestCallHonoursContextDeadline(t *testing.T) {
	c := clientFor(t, blockingServer(t))

	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	defer cancel()

	err := c.call(ctx, "getHealth", nil, nil)
	if !errors.Is(err, ErrRPC) {
		t.Fatalf("expected ErrRPC, got %v", err)
	}
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("expected context.DeadlineExceeded to be reachable, got %v", err)
	}
}

func TestCallHonoursHTTPClientTimeout(t *testing.T) {
	c := clientFor(t, blockingServer(t)).WithHTTPClient(&http.Client{Timeout: 20 * time.Millisecond})

	err := c.call(context.Background(), "getHealth", nil, nil)
	if !errors.Is(err, ErrRPC) {
		t.Fatalf("expected ErrRPC, got %v", err)
	}
	var netErr net.Error
	if !errors.As(err, &netErr) || !netErr.Timeout() {
		t.Fatalf("expected a timeout net.Error to be reachable, got %v", err)
	}
}

func TestCallWithAlreadyCancelledContextSendsNothing(t *testing.T) {
	hit := false
	srv := httptest.NewServer(http.HandlerFunc(func(http.ResponseWriter, *http.Request) { hit = true }))
	t.Cleanup(srv.Close)

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if err := clientFor(t, srv.URL).call(ctx, "getHealth", nil, nil); !errors.Is(err, context.Canceled) {
		t.Fatalf("expected context.Canceled, got %v", err)
	}
	if hit {
		t.Fatal("a cancelled context must not reach the server")
	}
}

func TestWithHTTPClientIgnoresNil(t *testing.T) {
	c := clientFor(t, "http://127.0.0.1:1")
	before := c.http
	if c.WithHTTPClient(nil).http != before {
		t.Fatal("WithHTTPClient(nil) must keep the existing client")
	}
}

// ── simulate ──────────────────────────────────────────────────────────────────

func TestSimulateResults(t *testing.T) {
	good, err := EncodeScValBase64(U32(9))
	if err != nil {
		t.Fatal(err)
	}
	cases := []struct {
		name     string
		result   map[string]interface{}
		wantXDR  string
		sentinel error
	}{
		{"single result", map[string]interface{}{"results": []map[string]string{{"xdr": good}}}, good, nil},
		{"first of several results", map[string]interface{}{"results": []map[string]string{{"xdr": good}, {"xdr": "other"}}}, good, nil},
		{"no results", map[string]interface{}{"results": []map[string]string{}}, "", ErrRPC},
		{"results absent", map[string]interface{}{"latestLedger": 7}, "", ErrRPC},
		{"empty xdr", map[string]interface{}{"results": []map[string]string{{"xdr": ""}}}, "", ErrRPC},
		{"contract error", map[string]interface{}{"error": "HostError: Error(Contract, #10)"}, "", ErrEmptyPool},
		{"error wins over results", map[string]interface{}{"error": "Error(Contract, #6)", "results": []map[string]string{{"xdr": good}}}, "", ErrPaused},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			rec := newRecordedRPC(t).respond("simulateTransaction", tc.result)
			c := newTestClient(t, rec, nil)

			got, err := c.simulate(context.Background(), zeroContract, "get_info", nil)
			if tc.sentinel != nil {
				if !errors.Is(err, tc.sentinel) {
					t.Fatalf("expected %v, got %v", tc.sentinel, err)
				}
				return
			}
			if err != nil {
				t.Fatalf("simulate: %v", err)
			}
			if got != tc.wantXDR {
				t.Fatalf("xdr = %q, want %q", got, tc.wantXDR)
			}
		})
	}
}

func TestSimulateUnmappedContractErrorIsStillTyped(t *testing.T) {
	rec := newRecordedRPC(t).respond("simulateTransaction", map[string]interface{}{"error": "HostError: Error(Budget, ExceededLimit)"})
	c := newTestClient(t, rec, nil)

	_, err := c.simulate(context.Background(), zeroContract, "get_info", nil)
	var ce *ContractError
	if !errors.As(err, &ce) {
		t.Fatalf("expected a *ContractError, got %T %v", err, err)
	}
	if ce.Code != 0 || !strings.Contains(ce.Raw, "ExceededLimit") {
		t.Fatalf("decoded %+v", ce)
	}
}

func TestSimulateRejectsBadContractIDBeforeRPC(t *testing.T) {
	c := clientFor(t, "http://127.0.0.1:1")
	for _, id := range []string{"", zeroAccount, "CNOTACONTRACT"} {
		if _, err := c.simulate(context.Background(), id, "get_info", nil); !errors.Is(err, ErrInvalidConfig) {
			t.Fatalf("id %q: expected ErrInvalidConfig, got %v", id, err)
		}
	}
}

func TestSimulateRejectsUnencodableArgs(t *testing.T) {
	c := clientFor(t, "http://127.0.0.1:1")
	_, err := c.simulate(context.Background(), zeroContract, "get_amount_out", []ScValue{I128(new(big.Int).Lsh(big.NewInt(1), 200))})
	if !errors.Is(err, ErrI128OutOfRange) {
		t.Fatalf("expected ErrI128OutOfRange, got %v", err)
	}
}

func TestSimulateSendsTheEnvelope(t *testing.T) {
	rec := newRecordedRPC(t).respond("simulateTransaction", simulationOf(t, U32(1)))
	c := newTestClient(t, rec, nil)

	if _, err := c.simulate(context.Background(), zeroContract, "get_info", nil); err != nil {
		t.Fatalf("simulate: %v", err)
	}
	var params struct {
		Transaction string `json:"transaction"`
	}
	if err := json.Unmarshal(rec.requests["simulateTransaction"], &params); err != nil {
		t.Fatalf("decoding params: %v", err)
	}
	want, err := BuildInvokeEnvelope(InvokeSpec{
		SourceAccount:     DefaultSimulationAccount,
		ContractID:        zeroContract,
		Method:            "get_info",
		NetworkPassphrase: NetworkTestnet,
	})
	if err != nil {
		t.Fatalf("BuildInvokeEnvelope: %v", err)
	}
	if params.Transaction != want {
		t.Fatal("simulate must send the envelope built for the simulation account")
	}
}

// ── invoke / pollTransaction ──────────────────────────────────────────────────

func TestInvokeSendStatuses(t *testing.T) {
	cases := []struct {
		status   string
		sentinel error
	}{
		{"PENDING", nil},
		{"DUPLICATE", nil},
		{"ERROR", ErrTransactionFailed},
		{"TRY_AGAIN_LATER", ErrTransactionFailed},
		{"", ErrTransactionFailed},
		{"pending", ErrTransactionFailed}, // send statuses are matched exactly
	}
	for _, tc := range cases {
		t.Run("status "+tc.status, func(t *testing.T) {
			rec := newRecordedRPC(t).
				respond("simulateTransaction", simulationOf(t, I128(big.NewInt(1)))).
				respond("sendTransaction", map[string]interface{}{"status": tc.status, "hash": "h", "errorResultXdr": "AAAAerr"}).
				respond("getTransaction", map[string]interface{}{"status": "SUCCESS"})
			c := newTestClient(t, rec, &testSigner{addr: zeroAccount})

			res, err := c.invoke(context.Background(), zeroContract, "swap", nil)
			if tc.sentinel == nil {
				if err != nil {
					t.Fatalf("invoke: %v", err)
				}
				if res.Hash != "h" || res.Status != "SUCCESS" {
					t.Fatalf("result = %+v", res)
				}
				return
			}
			if !errors.Is(err, tc.sentinel) {
				t.Fatalf("expected %v, got %v", tc.sentinel, err)
			}
			if tc.status == "ERROR" && !strings.Contains(err.Error(), "AAAAerr") {
				t.Fatalf("an ERROR rejection should carry the result xdr, got %q", err.Error())
			}
			for _, call := range rec.calls {
				if call == "getTransaction" {
					t.Fatal("a rejected send must not be polled")
				}
			}
		})
	}
}

func TestInvokeStopsAtEachFailingStage(t *testing.T) {
	cases := []struct {
		name      string
		rec       func(t *testing.T) *recordedRPC
		sentinel  error
		notCalled string
	}{
		{
			name: "simulate rpc error",
			rec: func(t *testing.T) *recordedRPC {
				return newRecordedRPC(t).on("simulateTransaction", func(json.RawMessage) (interface{}, *rpcError) {
					return nil, &rpcError{Code: -32000, Message: "down"}
				})
			},
			sentinel:  ErrRPC,
			notCalled: "sendTransaction",
		},
		{
			name: "simulate contract error",
			rec: func(t *testing.T) *recordedRPC {
				return newRecordedRPC(t).respond("simulateTransaction", map[string]interface{}{"error": "Error(Contract, #14)"})
			},
			sentinel:  ErrReentrant,
			notCalled: "sendTransaction",
		},
		{
			name: "send rpc error",
			rec: func(t *testing.T) *recordedRPC {
				return newRecordedRPC(t).
					respond("simulateTransaction", simulationOf(t, Void())).
					on("sendTransaction", func(json.RawMessage) (interface{}, *rpcError) {
						return nil, &rpcError{Code: -32001, Message: "tx malformed"}
					})
			},
			sentinel:  ErrRPC,
			notCalled: "getTransaction",
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			rec := tc.rec(t)
			c := newTestClient(t, rec, &testSigner{addr: zeroAccount})

			if _, err := c.invoke(context.Background(), zeroContract, "swap", nil); !errors.Is(err, tc.sentinel) {
				t.Fatalf("expected %v, got %v", tc.sentinel, err)
			}
			for _, call := range rec.calls {
				if call == tc.notCalled {
					t.Fatalf("%s must not be called after the failure", tc.notCalled)
				}
			}
		})
	}
}

func TestInvokeRejectsBeforeRPC(t *testing.T) {
	c := clientFor(t, "http://127.0.0.1:1")
	if _, err := c.invoke(context.Background(), zeroContract, "swap", nil); !errors.Is(err, ErrNoSigner) {
		t.Fatalf("expected ErrNoSigner, got %v", err)
	}

	c.signer = &testSigner{addr: zeroAccount}
	if _, err := c.invoke(context.Background(), "not-a-contract", "swap", nil); !errors.Is(err, ErrInvalidConfig) {
		t.Fatalf("expected ErrInvalidConfig, got %v", err)
	}
}

func TestPollTransactionStatuses(t *testing.T) {
	cases := []struct {
		status   string
		sentinel error
	}{
		{"SUCCESS", nil},
		{"success", nil}, // getTransaction statuses are matched case-insensitively
		{"FAILED", ErrTransactionFailed},
		{"failed", ErrTransactionFailed},
		{"BOGUS", ErrTransactionFailed},
		{"", ErrTransactionFailed},
	}
	for _, tc := range cases {
		t.Run("status "+tc.status, func(t *testing.T) {
			rec := newRecordedRPC(t).respond("getTransaction", map[string]interface{}{
				"status": tc.status, "returnValue": "AAAAAw==", "resultXdr": "AAAAfail",
			})
			c := newTestClient(t, rec, nil)

			res, err := c.pollTransaction(context.Background(), "h", "swap")
			if tc.sentinel == nil {
				if err != nil {
					t.Fatalf("pollTransaction: %v", err)
				}
				if *res != (TxResult{Hash: "h", Status: "SUCCESS", ReturnValue: "AAAAAw=="}) {
					t.Fatalf("result = %+v", res)
				}
				return
			}
			if !errors.Is(err, tc.sentinel) {
				t.Fatalf("expected %v, got %v", tc.sentinel, err)
			}
			if strings.EqualFold(tc.status, "FAILED") && !strings.Contains(err.Error(), "AAAAfail") {
				t.Fatalf("a FAILED transaction should carry the result xdr, got %q", err.Error())
			}
		})
	}
}

func TestPollTransactionBudget(t *testing.T) {
	polls := 0
	rec := newRecordedRPC(t).on("getTransaction", func(json.RawMessage) (interface{}, *rpcError) {
		polls++
		return map[string]interface{}{"status": "NOT_FOUND"}, nil
	})
	c := newTestClient(t, rec, nil).WithPolling(time.Millisecond, 3)

	_, err := c.pollTransaction(context.Background(), "h", "swap")
	if !errors.Is(err, ErrTransactionFailed) || !strings.Contains(err.Error(), "not confirmed after 3 polls") {
		t.Fatalf("expected a poll-budget failure, got %v", err)
	}
	// Attempt 0 plus three retries.
	if polls != 4 {
		t.Fatalf("polled %d times, want 4", polls)
	}
}

func TestPollTransactionSurfacesRPCErrorMidPoll(t *testing.T) {
	polls := 0
	rec := newRecordedRPC(t).on("getTransaction", func(json.RawMessage) (interface{}, *rpcError) {
		polls++
		if polls == 1 {
			return map[string]interface{}{"status": "NOT_FOUND"}, nil
		}
		return nil, &rpcError{Code: -32603, Message: "internal"}
	})
	c := newTestClient(t, rec, nil)

	if _, err := c.pollTransaction(context.Background(), "h", "swap"); !errors.Is(err, ErrRPC) {
		t.Fatalf("expected ErrRPC, got %v", err)
	}
}

func TestPollTransactionReturnsContextErrorBetweenPolls(t *testing.T) {
	rec := newRecordedRPC(t).respond("getTransaction", map[string]interface{}{"status": "NOT_FOUND"})
	c := newTestClient(t, rec, nil).WithPolling(time.Hour, 100)

	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	defer cancel()

	_, err := c.pollTransaction(ctx, "h", "swap")
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("expected context.DeadlineExceeded, got %v", err)
	}
}

func TestWithPollingIgnoresNonPositive(t *testing.T) {
	c := clientFor(t, "http://127.0.0.1:1").WithPolling(7*time.Millisecond, 9)
	c.WithPolling(0, 0).WithPolling(-time.Second, -1)
	if c.pollInterval != 7*time.Millisecond || c.pollAttempts != 9 {
		t.Fatalf("polling = (%v, %d), want (7ms, 9)", c.pollInterval, c.pollAttempts)
	}
}

func TestTruncate(t *testing.T) {
	cases := []struct {
		in   string
		n    int
		want string
	}{
		{"", 5, ""},
		{"abc", 5, "abc"},
		{"abcde", 5, "abcde"},
		{"abcdef", 5, "abcde..."},
		{"abc", 0, "..."},
	}
	for _, tc := range cases {
		if got := truncate(tc.in, tc.n); got != tc.want {
			t.Fatalf("truncate(%q, %d) = %q, want %q", tc.in, tc.n, got, tc.want)
		}
	}
}
