package gosdk

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"
)

// rpcRequest is a JSON-RPC 2.0 request envelope.
type rpcRequest struct {
	JSONRPC string      `json:"jsonrpc"`
	ID      int         `json:"id"`
	Method  string      `json:"method"`
	Params  interface{} `json:"params,omitempty"`
}

// rpcError is a JSON-RPC 2.0 error object.
type rpcError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

// rpcResponse is a JSON-RPC 2.0 response envelope with a deferred result.
type rpcResponse struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      int             `json:"id"`
	Result  json.RawMessage `json:"result"`
	Error   *rpcError       `json:"error"`
}

// SimulateResult is the subset of a simulateTransaction response this client
// consumes.
type SimulateResult struct {
	// Error carries the simulation failure text, empty on success.
	Error string `json:"error"`
	// Results holds one entry per host function invoked; XDR is the base64
	// return value.
	Results []struct {
		XDR string `json:"xdr"`
	} `json:"results"`
	// LatestLedger is the ledger the simulation ran against.
	LatestLedger uint32 `json:"latestLedger"`
	// TransactionData is the base64 SorobanTransactionData to attach to the
	// transaction before submission.
	TransactionData string `json:"transactionData"`
	// MinResourceFee is the additional resource fee simulation computed.
	MinResourceFee string `json:"minResourceFee"`
}

// SendResult is the subset of a sendTransaction response this client consumes.
type SendResult struct {
	// Status is PENDING, DUPLICATE, TRY_AGAIN_LATER or ERROR.
	Status string `json:"status"`
	// Hash is the submitted transaction's hash.
	Hash string `json:"hash"`
	// ErrorResultXDR carries the failure detail when Status is ERROR.
	ErrorResultXDR string `json:"errorResultXdr"`
}

// GetTxResult is the subset of a getTransaction response this client consumes.
type GetTxResult struct {
	// Status is NOT_FOUND, SUCCESS or FAILED.
	Status string `json:"status"`
	// ReturnValue is the base64 XDR of the invocation's return value.
	ReturnValue string `json:"returnValue"`
	// ResultXDR carries the failure detail when Status is FAILED.
	ResultXDR string `json:"resultXdr"`
}

// call performs one JSON-RPC round trip and unmarshals the result into out.
func (c *Client) call(ctx context.Context, method string, params interface{}, out interface{}) error {
	body, err := json.Marshal(rpcRequest{JSONRPC: "2.0", ID: 1, Method: method, Params: params})
	if err != nil {
		return fmt.Errorf("%w: encoding %s request: %v", ErrRPC, method, err)
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.rpcURL, bytes.NewReader(body))
	if err != nil {
		return fmt.Errorf("%w: building %s request: %v", ErrRPC, method, err)
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := c.http.Do(req)
	if err != nil {
		// Wrap the transport error as well, so callers can tell a cancelled or
		// timed-out context apart from an endpoint failure with errors.Is.
		return fmt.Errorf("%w: %s: %w", ErrRPC, method, err)
	}
	defer resp.Body.Close()

	raw, err := io.ReadAll(io.LimitReader(resp.Body, maxResponseBytes))
	if err != nil {
		return fmt.Errorf("%w: reading %s response: %v", ErrRPC, method, err)
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return fmt.Errorf("%w: %s: http %d: %s", ErrRPC, method, resp.StatusCode, truncate(string(raw), 256))
	}

	var envelope rpcResponse
	if err := json.Unmarshal(raw, &envelope); err != nil {
		return fmt.Errorf("%w: decoding %s response: %v", ErrRPC, method, err)
	}
	if envelope.Error != nil {
		return fmt.Errorf("%w: %s: %d %s", ErrRPC, method, envelope.Error.Code, envelope.Error.Message)
	}
	if out == nil {
		return nil
	}
	if err := json.Unmarshal(envelope.Result, out); err != nil {
		return fmt.Errorf("%w: decoding %s result: %v", ErrRPC, method, err)
	}
	return nil
}

// maxResponseBytes caps how much of an RPC response is read, so a hostile or
// broken endpoint cannot exhaust memory.
const maxResponseBytes = 16 << 20

// simulate invokes a contract method read-only and returns the base64 XDR of
// its return value. A contract-returned error is decoded into a *ContractError.
func (c *Client) simulate(ctx context.Context, contractID, method string, args []ScValue) (string, error) {
	if !IsContractAddress(contractID) {
		return "", fmt.Errorf("%w: contract id %q", ErrInvalidConfig, contractID)
	}

	envelope, err := BuildInvokeEnvelope(InvokeSpec{
		SourceAccount:     c.sourceAccount,
		ContractID:        contractID,
		Method:            method,
		Args:              args,
		NetworkPassphrase: c.networkPassphrase,
	})
	if err != nil {
		return "", err
	}

	var sim SimulateResult
	if err := c.call(ctx, "simulateTransaction", map[string]interface{}{"transaction": envelope}, &sim); err != nil {
		return "", err
	}
	if sim.Error != "" {
		return "", DecodeContractError(sim.Error)
	}
	if len(sim.Results) == 0 || sim.Results[0].XDR == "" {
		return "", fmt.Errorf("%w: %s returned no result", ErrRPC, method)
	}
	return sim.Results[0].XDR, nil
}

// invoke simulates, signs, submits and polls a state-changing contract call.
func (c *Client) invoke(ctx context.Context, contractID, method string, args []ScValue) (*TxResult, error) {
	if c.signer == nil {
		return nil, ErrNoSigner
	}
	if !IsContractAddress(contractID) {
		return nil, fmt.Errorf("%w: contract id %q", ErrInvalidConfig, contractID)
	}

	envelope, err := BuildInvokeEnvelope(InvokeSpec{
		SourceAccount:     c.signer.Address(),
		ContractID:        contractID,
		Method:            method,
		Args:              args,
		NetworkPassphrase: c.networkPassphrase,
	})
	if err != nil {
		return nil, err
	}

	var sim SimulateResult
	if err := c.call(ctx, "simulateTransaction", map[string]interface{}{"transaction": envelope}, &sim); err != nil {
		return nil, err
	}
	if sim.Error != "" {
		return nil, DecodeContractError(sim.Error)
	}

	signed, err := c.signer.SignEnvelope(ctx, envelope, c.networkPassphrase)
	if err != nil {
		return nil, fmt.Errorf("signing %s: %w", method, err)
	}

	var send SendResult
	if err := c.call(ctx, "sendTransaction", map[string]interface{}{"transaction": signed}, &send); err != nil {
		return nil, err
	}
	switch send.Status {
	case "PENDING", "DUPLICATE":
	case "ERROR":
		return nil, fmt.Errorf("%w: %s rejected: %s", ErrTransactionFailed, method, send.ErrorResultXDR)
	default:
		return nil, fmt.Errorf("%w: %s: unexpected send status %q", ErrTransactionFailed, method, send.Status)
	}

	return c.pollTransaction(ctx, send.Hash, method)
}

// pollTransaction polls getTransaction until the transaction leaves NOT_FOUND
// or the context is cancelled.
func (c *Client) pollTransaction(ctx context.Context, hash, method string) (*TxResult, error) {
	for attempt := 0; ; attempt++ {
		var got GetTxResult
		if err := c.call(ctx, "getTransaction", map[string]interface{}{"hash": hash}, &got); err != nil {
			return nil, err
		}

		switch strings.ToUpper(got.Status) {
		case "SUCCESS":
			return &TxResult{Hash: hash, Status: "SUCCESS", ReturnValue: got.ReturnValue}, nil
		case "FAILED":
			return nil, fmt.Errorf("%w: %s: %s", ErrTransactionFailed, method, got.ResultXDR)
		case "NOT_FOUND":
			// Still in flight; fall through to the backoff below.
		default:
			return nil, fmt.Errorf("%w: %s: unexpected status %q", ErrTransactionFailed, method, got.Status)
		}

		if attempt >= c.pollAttempts {
			return nil, fmt.Errorf("%w: %s: not confirmed after %d polls", ErrTransactionFailed, method, attempt)
		}
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(c.pollInterval):
		}
	}
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}
