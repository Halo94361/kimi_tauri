package main

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"flag"
	"io"
	"log"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"syscall"
	"time"
)

const sessionHeader = "x-opencode-session"

type payload struct {
	PromptCacheKey string `json:"prompt_cache_key"`
	Metadata       struct {
		UserID string `json:"user_id"`
	} `json:"metadata"`
}

func main() {
	listen := flag.String("listen", env("OCSP_LISTEN", "127.0.0.1:8787"), "listen address")
	upstreamRaw := flag.String("upstream", env("OCSP_UPSTREAM", "https://opencode.ai/inference/go"), "upstream base URL")
	debug := flag.Bool("debug", env("OCSP_DEBUG", "") == "1", "log injected session ids")
	maxBodyMiB := flag.Int64("max-body", envInt("OCSP_MAX_BODY", 64), "max buffered request body in MiB")
	flag.Parse()

	maxBody := *maxBodyMiB << 20
	if maxBody <= 0 {
		maxBody = 64 << 20
	}

	upstream, err := url.Parse(*upstreamRaw)
	if err != nil || upstream.Scheme == "" || upstream.Host == "" {
		log.Fatalf("invalid upstream %q", *upstreamRaw)
	}

	fallback := "ses_proxy_" + randomHex(8)

	proxy := &httputil.ReverseProxy{
		Rewrite: func(pr *httputil.ProxyRequest) {
			inject(pr, fallback, maxBody, *debug)
			pr.Out.URL.Scheme = upstream.Scheme
			pr.Out.URL.Host = upstream.Host
			pr.Out.URL.Path = joinPath(upstream.Path, pr.In.URL.Path)
			pr.Out.URL.RawQuery = pr.In.URL.RawQuery
			pr.Out.Host = upstream.Host
			pr.SetXForwarded()
		},
		Transport: &http.Transport{
			Proxy: http.ProxyFromEnvironment,
			DialContext: (&net.Dialer{
				Timeout:   10 * time.Second,
				KeepAlive: 30 * time.Second,
			}).DialContext,
			ForceAttemptHTTP2:     true,
			MaxIdleConns:          100,
			IdleConnTimeout:       90 * time.Second,
			TLSHandshakeTimeout:   10 * time.Second,
			ResponseHeaderTimeout: 180 * time.Second,
			ExpectContinueTimeout: time.Second,
		},
		FlushInterval: -1,
		ErrorHandler: func(w http.ResponseWriter, r *http.Request, err error) {
			log.Printf("upstream error path=%s err=%v", r.URL.Path, err)
			w.WriteHeader(http.StatusBadGateway)
			_, _ = w.Write([]byte("opencode-session-proxy: upstream error\n"))
		},
	}

	srv := &http.Server{
		Addr:              *listen,
		Handler:           proxy,
		ReadHeaderTimeout: 10 * time.Second,
		IdleTimeout:       120 * time.Second,
	}

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	go func() {
		<-ctx.Done()
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		_ = srv.Shutdown(shutdownCtx)
	}()

	log.Printf("listening on %s -> %s (debug=%v)", *listen, upstream.String(), *debug)
	if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
		log.Fatal(err)
	}
}

func inject(pr *httputil.ProxyRequest, fallback string, maxBody int64, debug bool) {
	if pr.In.Body == nil || pr.In.Body == http.NoBody {
		return
	}

	// 已知长度且超限：不缓冲，原样流式转发，仅注入进程级 fallback
	if pr.In.ContentLength > maxBody {
		applySession(pr, fallback, "fallback(oversize)", debug)
		return
	}

	body, err := io.ReadAll(io.LimitReader(pr.In.Body, maxBody+1))
	if err != nil {
		return
	}

	// 未知长度（chunked）且超限：已读前缀 + 剩余流拼回去，避免截断请求体
	if int64(len(body)) > maxBody {
		pr.Out.Body = io.NopCloser(io.MultiReader(bytes.NewReader(body), pr.In.Body))
		pr.Out.ContentLength = -1
		pr.Out.Header.Del("Content-Length")
		applySession(pr, fallback, "fallback(oversize)", debug)
		return
	}

	pr.Out.Body = io.NopCloser(bytes.NewReader(body))
	pr.Out.ContentLength = int64(len(body))
	pr.Out.Header.Set("Content-Length", strconv.Itoa(len(body)))

	var p payload
	if json.Unmarshal(body, &p) != nil {
		applySession(pr, fallback, "fallback(unparsable)", debug)
		return
	}

	id, source := p.PromptCacheKey, "prompt_cache_key"
	if id == "" {
		id, source = p.Metadata.UserID, "metadata.user_id"
	}
	if id == "" {
		id, source = fallback, "fallback"
	}
	id = sanitize(id)
	if id == "" {
		id, source = fallback, "fallback"
	}
	applySession(pr, id, source, debug)
}

func applySession(pr *httputil.ProxyRequest, id, source string, debug bool) {
	delete(pr.Out.Header, sessionHeader)
	delete(pr.Out.Header, "X-Opencode-Session")
	pr.Out.Header[sessionHeader] = []string{id}
	if debug {
		log.Printf("inject path=%s session=%s source=%s", pr.In.URL.Path, id, source)
	}
}

func sanitize(s string) string {
	var b strings.Builder
	for _, r := range s {
		if r > 0x20 && r < 0x7f && r != '"' && r != '\\' {
			b.WriteRune(r)
		}
		if b.Len() >= 191 {
			break
		}
	}
	return b.String()
}

func joinPath(base, p string) string {
	if base == "" {
		return p
	}
	if p == "" {
		return base
	}
	return strings.TrimRight(base, "/") + "/" + strings.TrimLeft(p, "/")
}

func randomHex(n int) string {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		return strconv.FormatInt(time.Now().UnixNano(), 16)
	}
	return hex.EncodeToString(b)
}

func env(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

func envInt(key string, def int64) int64 {
	raw := os.Getenv(key)
	if raw == "" {
		return def
	}
	v, err := strconv.ParseInt(raw, 10, 64)
	if err != nil || v <= 0 {
		return def
	}
	return v
}
