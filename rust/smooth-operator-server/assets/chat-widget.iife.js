var SmoothAgentChat = (function(exports) {
	Object.defineProperty(exports, Symbol.toStringTag, { value: "Module" });
	//#region src/config.ts
	/** Resolve a partial config against the built-in defaults. */
	function resolveConfig(config) {
		const theme = config.theme ?? {};
		const primary = theme.primary ?? "#00a6a6";
		const primaryText = theme.primaryText ?? "#f8fafc";
		const assistantBubble = theme.chatBubbleInbound ?? theme.assistantBubble ?? "#06134b";
		const assistantBubbleText = theme.chatBubbleInboundText ?? theme.assistantBubbleText ?? "#f8fafc";
		const userBubble = theme.chatBubbleOutbound ?? theme.userBubble ?? primary;
		const userBubbleText = theme.chatBubbleOutboundText ?? theme.userBubbleText ?? primaryText;
		return {
			endpoint: config.endpoint,
			mode: config.mode ?? "popover",
			agentId: config.agentId,
			agentName: config.agentName ?? "Assistant",
			userName: config.userName,
			userEmail: config.userEmail,
			userPhone: config.userPhone,
			placeholder: config.placeholder ?? "Type a message…",
			greeting: config.greeting ?? "Hi! How can I help you today?",
			connectionErrorMessage: config.connectionErrorMessage ?? "We couldn't reach the chat. Please try again in a moment.",
			startOpen: config.startOpen ?? false,
			examplePrompts: (config.examplePrompts ?? []).filter((p) => p.trim().length > 0).slice(0, 5),
			requireName: config.requireName ?? false,
			requireEmail: config.requireEmail ?? false,
			requirePhone: config.requirePhone ?? false,
			allowAnonymous: config.allowAnonymous ?? false,
			theme: {
				text: theme.text ?? "#f8fafc",
				background: theme.background ?? "#040d30",
				primary,
				primaryText,
				secondary: theme.secondary ?? "#ff6b6c",
				assistantBubble,
				assistantBubbleText,
				userBubble,
				userBubbleText,
				border: theme.border ?? "rgba(255, 255, 255, 0.1)"
			}
		};
	}
	/**
	* Whether the pre-chat identity form should gate the conversation: at least one
	* field is required and anonymous chat is not allowed.
	*/
	function needsUserInfo(resolved) {
		return !resolved.allowAnonymous && (resolved.requireName || resolved.requireEmail || resolved.requirePhone);
	}
	//#endregion
	//#region node_modules/.pnpm/@smooai+smooth-operator@0.2.0/node_modules/@smooai/smooth-operator/dist/transport.js
	/**
	* Transport abstraction for the client.
	*
	* The client is deliberately decoupled from any concrete WebSocket implementation
	* so it can be unit-tested with a mock and run on Node, the browser, or a custom
	* socket. A transport is anything that can send a string frame and surface
	* incoming string frames + lifecycle events.
	*/
	const WS_CONNECTING = 0;
	const WS_OPEN = 1;
	const WS_CLOSING = 2;
	/** Default connect timeout (ms) for the WebSocket transport. */
	const DEFAULT_CONNECT_TIMEOUT = 3e4;
	/**
	* Default transport backed by a `WebSocket`-like object. By default it uses the
	* global `WebSocket`; pass a `factory` to inject one (e.g. the `ws` package on
	* Node, or a mock in tests).
	*/
	var WebSocketTransport = class {
		socket = null;
		url;
		factory;
		connectTimeout;
		messageHandlers = /* @__PURE__ */ new Set();
		closeHandlers = /* @__PURE__ */ new Set();
		errorHandlers = /* @__PURE__ */ new Set();
		constructor(url, factory, connectTimeout = DEFAULT_CONNECT_TIMEOUT) {
			this.url = url;
			this.connectTimeout = connectTimeout;
			if (factory) this.factory = factory;
			else {
				const G = globalThis;
				if (!G.WebSocket) throw new Error("No global WebSocket available; pass a WebSocketFactory to WebSocketTransport.");
				const Ctor = G.WebSocket;
				this.factory = (u) => new Ctor(u);
			}
		}
		get state() {
			if (!this.socket) return "closed";
			switch (this.socket.readyState) {
				case WS_CONNECTING: return "connecting";
				case WS_OPEN: return "open";
				case WS_CLOSING: return "closing";
				default: return "closed";
			}
		}
		connect() {
			if (this.socket && this.socket.readyState === WS_OPEN) return Promise.resolve();
			if (this.socket && this.socket.readyState !== WS_OPEN) {
				const stale = this.socket;
				this.socket = null;
				try {
					stale.close();
				} catch {}
			}
			return new Promise((resolve, reject) => {
				const socket = this.factory(this.url);
				this.socket = socket;
				let settled = false;
				const timer = this.connectTimeout > 0 ? setTimeout(() => {
					if (settled) return;
					settled = true;
					if (this.socket === socket) this.socket = null;
					try {
						socket.close();
					} catch {}
					reject(/* @__PURE__ */ new Error(`WebSocket connect to ${this.url} timed out after ${this.connectTimeout}ms`));
				}, this.connectTimeout) : void 0;
				socket.addEventListener("open", () => {
					if (this.socket !== socket) return;
					if (settled) return;
					settled = true;
					if (timer) clearTimeout(timer);
					resolve();
				});
				socket.addEventListener("error", (ev) => {
					if (this.socket !== socket) return;
					for (const h of this.errorHandlers) h(ev);
					if (!settled && this.state !== "open") {
						settled = true;
						if (timer) clearTimeout(timer);
						if (this.socket === socket) this.socket = null;
						try {
							socket.close();
						} catch {}
						reject(ev instanceof Error ? ev : /* @__PURE__ */ new Error("WebSocket connection error"));
					}
				});
				socket.addEventListener("close", (ev) => {
					if (this.socket !== socket) return;
					if (timer) clearTimeout(timer);
					for (const h of this.closeHandlers) h({
						code: ev.code,
						reason: ev.reason
					});
				});
				socket.addEventListener("message", (ev) => {
					if (this.socket !== socket) return;
					const data = typeof ev.data === "string" ? ev.data : String(ev.data);
					for (const h of this.messageHandlers) h(data);
				});
			});
		}
		send(data) {
			if (!this.socket || this.socket.readyState !== WS_OPEN) throw new Error(`Cannot send: transport is "${this.state}"`);
			this.socket.send(data);
		}
		close(code, reason) {
			this.socket?.close(code, reason);
		}
		onMessage(handler) {
			this.messageHandlers.add(handler);
			return () => this.messageHandlers.delete(handler);
		}
		onClose(handler) {
			this.closeHandlers.add(handler);
			return () => this.closeHandlers.delete(handler);
		}
		onError(handler) {
			this.errorHandlers.add(handler);
			return () => this.errorHandlers.delete(handler);
		}
	};
	//#endregion
	//#region node_modules/.pnpm/@smooai+smooth-operator@0.2.0/node_modules/@smooai/smooth-operator/dist/types.js
	/** Every server→client `type` discriminator value. */
	const EVENT_TYPES = [
		"immediate_response",
		"eventual_response",
		"stream_chunk",
		"stream_token",
		"keepalive",
		"write_confirmation_required",
		"otp_verification_required",
		"otp_sent",
		"otp_verified",
		"otp_invalid",
		"error",
		"pong"
	];
	/** True if `frame` looks like any server event (has a known `type` discriminator). */
	function isServerEvent(frame) {
		return typeof frame === "object" && frame !== null && "type" in frame && typeof frame.type === "string" && EVENT_TYPES.includes(frame.type);
	}
	//#endregion
	//#region node_modules/.pnpm/@smooai+smooth-operator@0.2.0/node_modules/@smooai/smooth-operator/dist/client.js
	/**
	* SmoothAgentClient — a minimal, idiomatic, transport-agnostic client for the
	* smooth-operator WebSocket protocol.
	*
	* Design goals
	* ------------
	*  - **Transport-agnostic.** The client never touches a real socket directly; it
	*    talks to an injectable {@link Transport}. The default ({@link WebSocketTransport})
	*    uses the global `WebSocket`, but tests inject a mock and Node can inject `ws`.
	*  - **Request/response correlation by `requestId`.** Every action gets a generated
	*    `requestId`; the client routes incoming events back to the originating call.
	*  - **Streaming as an async iterator.** `sendMessage` returns a {@link MessageTurn}
	*    that is both awaitable (resolves with the terminal `eventual_response`) and
	*    async-iterable (yields each `stream_token` / `stream_chunk` / HITL event in
	*    order). This models the `stream_token`/`stream_chunk` → `eventual_response`
	*    flow without forcing a callback style on the caller.
	*  - **No live server required.** Correctness is fully unit-testable with a mock
	*    transport (see `test/client.test.ts`).
	*/
	/** A timeout that yields no terminal event. */
	var RequestTimeoutError = class extends Error {
		constructor(requestId, ms) {
			super(`Request ${requestId} timed out after ${ms}ms`);
			this.name = "RequestTimeoutError";
		}
	};
	/**
	* A streaming turn that received no terminal `eventual_response` / `error` within the
	* configured {@link SmoothAgentClientOptions.turnTimeout}. The turn rejects with this
	* and its async iteration throws it, so a stuck server can never hang the caller.
	*/
	var TurnTimeoutError = class extends Error {
		requestId;
		constructor(requestId, ms) {
			super(`Turn ${requestId} timed out after ${ms}ms without a terminal response`);
			this.name = "TurnTimeoutError";
			this.requestId = requestId;
		}
	};
	/** A protocol-level error event surfaced as a throwable. */
	var ProtocolError = class extends Error {
		code;
		requestId;
		constructor(code, message, requestId) {
			super(message);
			this.name = "ProtocolError";
			this.code = code;
			this.requestId = requestId;
		}
	};
	/**
	* A streaming message turn. Await it for the terminal {@link EventualResponse},
	* or async-iterate it to receive every intermediate event in arrival order.
	*
	* ```ts
	* const turn = client.sendMessage({ sessionId, message: 'hi' });
	* for await (const ev of turn) {
	*   if (ev.type === 'stream_token') process.stdout.write(ev.token ?? '');
	* }
	* const final = await turn; // EventualResponse
	* ```
	*/
	var MessageTurn = class {
		/** The requestId this turn is correlated on. */
		requestId;
		queue = [];
		waiter = null;
		done = false;
		finalEvent = null;
		error = null;
		settled;
		settle;
		fail;
		onClose;
		timeoutTimer;
		constructor(requestId, onClose, turnTimeout = 0) {
			this.requestId = requestId;
			this.onClose = onClose;
			this.settled = new Promise((resolve, reject) => {
				this.settle = resolve;
				this.fail = reject;
			});
			this.settled.catch(() => {});
			if (turnTimeout > 0) this.timeoutTimer = setTimeout(() => {
				this.finish(null, new TurnTimeoutError(this.requestId, turnTimeout));
			}, turnTimeout);
		}
		/** Feed an event into the turn (called by the client's dispatcher). */
		push(event) {
			if (this.done) return;
			if (event.type === "error") {
				const code = event.data?.error?.code ?? "INTERNAL_ERROR";
				const message = event.data?.error?.message ?? "Unknown protocol error";
				this.deliver(event);
				this.finish(null, new ProtocolError(code, message, this.requestId));
				return;
			}
			this.deliver(event);
			if (event.type === "eventual_response") this.finish(event, null);
		}
		/** Force-close the turn (e.g. on disconnect) with an error. */
		abort(err) {
			if (this.done) return;
			this.finish(null, err);
		}
		deliver(event) {
			if (this.waiter) {
				const w = this.waiter;
				this.waiter = null;
				w.resolve({
					value: event,
					done: false
				});
			} else this.queue.push(event);
		}
		finish(final, err) {
			if (this.done) return;
			this.done = true;
			this.finalEvent = final;
			this.error = err;
			if (this.timeoutTimer) {
				clearTimeout(this.timeoutTimer);
				this.timeoutTimer = void 0;
			}
			this.onClose();
			if (err) this.fail(err);
			else if (final) this.settle(final);
			if (this.waiter) {
				const w = this.waiter;
				this.waiter = null;
				if (err) w.reject(err);
				else w.resolve({
					value: void 0,
					done: true
				});
			}
		}
		[Symbol.asyncIterator]() {
			return { next: () => {
				if (this.queue.length > 0) return Promise.resolve({
					value: this.queue.shift(),
					done: false
				});
				if (this.done) {
					if (this.error) return Promise.reject(this.error);
					return Promise.resolve({
						value: void 0,
						done: true
					});
				}
				return new Promise((resolve, reject) => {
					this.waiter = {
						resolve,
						reject
					};
				});
			} };
		}
		then(onfulfilled, onrejected) {
			return this.settled.then(onfulfilled, onrejected);
		}
	};
	var SmoothAgentClient = class {
		transport;
		generateRequestId;
		requestTimeout;
		turnTimeout;
		/** requestId → single-response waiter (create_session, get_session, ping, …). */
		pending = /* @__PURE__ */ new Map();
		/** requestId → active streaming turn (send_message, and HITL resumes). */
		turns = /* @__PURE__ */ new Map();
		/** Unsolicited-event listeners (keepalive, server-push). */
		listeners = /* @__PURE__ */ new Set();
		unsubscribe = [];
		constructor(options) {
			this.transport = options.transport ?? new WebSocketTransport(options.url, options.webSocketFactory);
			this.requestTimeout = options.requestTimeout ?? 3e4;
			this.turnTimeout = options.turnTimeout ?? 12e4;
			this.generateRequestId = options.generateRequestId ?? (() => `req-${globalThis.crypto?.randomUUID?.() ?? Math.random().toString(36).slice(2)}`);
			this.unsubscribe.push(this.transport.onMessage((data) => this.handleFrame(data)));
			this.unsubscribe.push(this.transport.onClose(() => this.failAll(/* @__PURE__ */ new Error("Transport closed"))));
		}
		/** Open the underlying transport. */
		async connect() {
			await this.transport.connect();
		}
		/** Close the transport and reject all in-flight work. */
		disconnect(reason = "client disconnect") {
			this.failAll(new Error(reason));
			for (const u of this.unsubscribe) u();
			this.unsubscribe = [];
			this.transport.close(1e3, reason);
		}
		/** Subscribe to unsolicited / uncorrelated server events (e.g. keepalive). */
		onEvent(listener) {
			this.listeners.add(listener);
			return () => this.listeners.delete(listener);
		}
		/** Start a new conversation session. Resolves with the session descriptor. */
		async createConversationSession(req) {
			return extractImmediateData(await this.request({
				action: "create_conversation_session",
				...req
			}));
		}
		/** Fetch a session snapshot by ID. */
		async getSession(req) {
			return extractImmediateData(await this.request({
				action: "get_session",
				...req
			}));
		}
		/** Fetch a page of conversation messages. */
		async getMessages(req) {
			return extractImmediateData(await this.request({
				action: "get_conversation_messages",
				...req
			}));
		}
		/** Keepalive ping. Resolves with the server timestamp from the `pong` event. */
		async ping() {
			const event = await this.request({ action: "ping" });
			if (event.type === "pong") return event.timestamp ?? event.data?.timestamp ?? Date.now();
			return Date.now();
		}
		/**
		* Submit a user message and return a {@link MessageTurn}: await it for the
		* terminal `eventual_response`, or async-iterate it for the streaming events.
		*/
		sendMessage(req) {
			const requestId = this.generateRequestId();
			const turn = new MessageTurn(requestId, () => this.turns.delete(requestId), this.turnTimeout);
			this.turns.set(requestId, turn);
			try {
				this.transport.send(JSON.stringify({
					action: "send_message",
					requestId,
					...req
				}));
			} catch (err) {
				this.turns.delete(requestId);
				turn.abort(err);
			}
			return turn;
		}
		/**
		* Approve or reject a pending tool write, resuming the paused turn identified
		* by `requestId`. The resumed streaming events flow back into the original
		* {@link MessageTurn} for that `requestId`.
		*/
		confirmToolAction(req) {
			this.transport.send(JSON.stringify({
				action: "confirm_tool_action",
				...req
			}));
		}
		/**
		* Submit an OTP code, resuming the paused turn identified by `requestId`.
		* The resumed streaming events flow back into the original {@link MessageTurn}.
		*/
		verifyOtp(req) {
			this.transport.send(JSON.stringify({
				action: "verify_otp",
				...req
			}));
		}
		/** Send an action that expects a single correlated response event. */
		request(action) {
			const requestId = action.requestId ?? this.generateRequestId();
			const frame = {
				...action,
				requestId
			};
			return new Promise((resolve, reject) => {
				const timer = this.requestTimeout > 0 ? setTimeout(() => {
					this.pending.delete(requestId);
					reject(new RequestTimeoutError(requestId, this.requestTimeout));
				}, this.requestTimeout) : void 0;
				this.pending.set(requestId, {
					resolve,
					reject,
					timer
				});
				try {
					this.transport.send(JSON.stringify(frame));
				} catch (err) {
					if (timer) clearTimeout(timer);
					this.pending.delete(requestId);
					reject(err);
				}
			});
		}
		/** Parse and route an incoming frame to the right consumer. */
		handleFrame(data) {
			let frame;
			try {
				frame = JSON.parse(data);
			} catch {
				return;
			}
			if (!isServerEvent(frame)) return;
			const event = frame;
			const requestId = event.requestId;
			if (requestId && this.turns.has(requestId)) {
				this.turns.get(requestId).push(event);
				return;
			}
			if (requestId && this.pending.has(requestId)) {
				const pending = this.pending.get(requestId);
				this.pending.delete(requestId);
				if (pending.timer) clearTimeout(pending.timer);
				if (event.type === "error") {
					const code = event.data?.error?.code ?? "INTERNAL_ERROR";
					const message = event.data?.error?.message ?? "Unknown protocol error";
					pending.reject(new ProtocolError(code, message, requestId));
				} else pending.resolve(event);
				return;
			}
			for (const l of this.listeners) l(event);
		}
		failAll(err) {
			for (const [, p] of this.pending) {
				if (p.timer) clearTimeout(p.timer);
				p.reject(err);
			}
			this.pending.clear();
			for (const [, turn] of this.turns) turn.abort(err);
			this.turns.clear();
		}
	};
	/** Pull the typed `data` payload out of an `immediate_response` event. */
	function extractImmediateData(event) {
		if (event.type === "immediate_response") return event.data;
		if ("data" in event && event.data && typeof event.data === "object") return event.data;
		throw new ProtocolError("UNEXPECTED_EVENT", `Expected immediate_response, got "${event.type}"`, event.requestId);
	}
	//#endregion
	//#region src/conversation.ts
	/**
	* ConversationController — the bridge between the widget UI and the
	* `@smooai/smooth-operator` protocol client.
	*
	* This is the piece that was rewired: the original smooai widget spoke to
	* `@smooai/realtime`; here every protocol action goes through {@link SmoothAgentClient}.
	* The wire shapes are identical (the protocol was lifted from `@smooai/realtime`),
	* so the swap is purely at the client-library boundary.
	*
	* Flow:
	*   1. `connect()`        → opens the WebSocket transport and `create_conversation_session`.
	*   2. `send(text)`       → `send_message`, streaming `stream_token` deltas into the
	*                           in-progress assistant message, then the terminal
	*                           `eventual_response`.
	*
	* The controller is UI-agnostic: it emits typed events and the view renders them.
	*/
	/** Pull the final assistant text out of an `eventual_response` data payload. */
	function extractFinalText(response) {
		if (!response || typeof response !== "object") return null;
		const r = response;
		if (Array.isArray(r.responseParts)) return r.responseParts.filter((p) => typeof p === "string").join("\n\n");
		return null;
	}
	/**
	* Pull the grounding {@link Citation}s out of a terminal `eventual_response`.
	*
	* The protocol client types these (`eventual_response.data.data.citations`),
	* but they're optional and back-compatible — absent when the turn used no
	* knowledge sources. We read them defensively (tolerating their total absence,
	* non-array shapes, and missing fields) so a server that doesn't emit them, or
	* an older client, can't break rendering. Each citation always carries
	* `id`/`title`/`snippet`/`score`; `url` is present only for web-sourced docs.
	*/
	function extractCitations(inner) {
		if (!inner || typeof inner !== "object") return [];
		const raw = inner.citations;
		if (!Array.isArray(raw)) return [];
		const out = [];
		for (const c of raw) {
			if (!c || typeof c !== "object") continue;
			const obj = c;
			const id = typeof obj.id === "string" ? obj.id : "";
			const title = typeof obj.title === "string" ? obj.title : id || "Source";
			const snippet = typeof obj.snippet === "string" ? obj.snippet : "";
			const url = typeof obj.url === "string" && obj.url ? obj.url : void 0;
			const score = typeof obj.score === "number" ? obj.score : 0;
			out.push({
				id,
				title,
				snippet,
				score,
				url
			});
		}
		return out;
	}
	var ConversationController = class {
		config;
		events;
		client = null;
		sessionId = null;
		messages = [];
		status = "idle";
		seq = 0;
		/** Visitor identity, seeded from config and updated by the pre-chat form. */
		identity;
		/** requestId of the in-flight turn — used to resume OTP / tool confirmations. */
		activeRequestId = null;
		interrupt = null;
		constructor(config, events) {
			this.config = config;
			this.events = events;
			this.identity = {
				name: config.userName,
				email: config.userEmail,
				phone: config.userPhone
			};
		}
		get connectionStatus() {
			return this.status;
		}
		/** Merge in visitor identity (from the pre-chat form). Applied on next connect. */
		setUserInfo(info) {
			this.identity = {
				...this.identity,
				...info
			};
		}
		setInterrupt(interrupt) {
			this.interrupt = interrupt;
			this.events.onInterrupt?.(interrupt);
		}
		/** Submit an OTP code to resume the paused turn. No-op if not awaiting OTP. */
		verifyOtp(code) {
			if (!this.client || !this.sessionId || !this.activeRequestId || this.interrupt?.kind !== "otp") return;
			this.client.verifyOtp({
				sessionId: this.sessionId,
				requestId: this.activeRequestId,
				code
			});
		}
		/** Approve or reject a pending tool write to resume the paused turn. */
		confirmTool(approved) {
			if (!this.client || !this.sessionId || !this.activeRequestId || this.interrupt?.kind !== "confirm") return;
			this.client.confirmToolAction({
				sessionId: this.sessionId,
				requestId: this.activeRequestId,
				approved
			});
			this.setInterrupt(null);
		}
		nextId(prefix) {
			this.seq += 1;
			return `${prefix}-${this.seq}-${Date.now().toString(36)}`;
		}
		setStatus(status, detail) {
			this.status = status;
			this.events.onStatus(status, detail);
		}
		emitMessages() {
			this.events.onMessages(this.messages.map((m) => ({ ...m })));
		}
		/** Open the transport and create a conversation session. Idempotent. */
		async connect() {
			if (this.status === "connecting" || this.status === "ready") return;
			this.setStatus("connecting");
			try {
				this.client = new SmoothAgentClient({ url: this.config.endpoint });
				await this.client.connect();
				const session = await this.client.createConversationSession({
					agentId: this.config.agentId,
					userName: this.identity.name,
					userEmail: this.identity.email,
					...this.identity.phone ? { metadata: { userPhone: this.identity.phone } } : {}
				});
				this.sessionId = session.sessionId;
				this.setStatus("ready");
			} catch (err) {
				this.setStatus("error", err instanceof Error ? err.message : String(err));
				throw err;
			}
		}
		/**
		* Submit a user message. Appends the user bubble immediately, then streams the
		* assistant reply token-by-token, finalizing on `eventual_response`.
		*/
		async send(text) {
			const trimmed = text.trim();
			if (!trimmed) return;
			if (!this.client || !this.sessionId || this.status !== "ready") await this.connect();
			if (!this.client || !this.sessionId) throw new Error("Conversation is not connected");
			this.messages.push({
				id: this.nextId("u"),
				role: "user",
				text: trimmed,
				streaming: false
			});
			const assistant = {
				id: this.nextId("a"),
				role: "assistant",
				text: "",
				streaming: true
			};
			this.messages.push(assistant);
			this.emitMessages();
			try {
				const turn = this.client.sendMessage({
					sessionId: this.sessionId,
					message: trimmed,
					stream: true
				});
				this.activeRequestId = turn.requestId;
				for await (const event of turn) if (event.type === "stream_token") {
					const token = event.token ?? event.data?.token ?? "";
					if (token) {
						assistant.text += token;
						this.emitMessages();
					}
				} else this.handleTurnEvent(event);
				const inner = (await turn).data?.data;
				const finalText = extractFinalText(inner?.response);
				if (finalText && finalText.length > assistant.text.length) assistant.text = finalText;
				if (!assistant.text) assistant.text = "(no response)";
				const citations = extractCitations(inner);
				if (citations.length > 0) assistant.citations = citations;
				assistant.streaming = false;
				this.emitMessages();
			} catch (err) {
				assistant.streaming = false;
				const message = err instanceof ProtocolError ? `Error: ${err.message}` : this.config.connectionErrorMessage ?? "We couldn't reach the chat.";
				assistant.text = assistant.text ? `${assistant.text}\n\n${message}` : message;
				this.emitMessages();
				this.setStatus("error", err instanceof Error ? err.message : String(err));
			} finally {
				this.activeRequestId = null;
				this.setInterrupt(null);
			}
		}
		/** Map a non-token turn event (OTP / tool-confirmation lifecycle) to interrupt state. */
		handleTurnEvent(event) {
			const inner = event.data?.data ?? {};
			const str = (v) => typeof v === "string" ? v : void 0;
			const num = (v) => typeof v === "number" ? v : void 0;
			switch (event.type) {
				case "otp_verification_required": {
					const channels = Array.isArray(inner.availableChannels) ? inner.availableChannels.filter((c) => c === "email" || c === "sms") : ["email"];
					this.setInterrupt({
						kind: "otp",
						toolId: str(inner.toolId),
						actionDescription: str(inner.actionDescription),
						availableChannels: channels.length > 0 ? channels : ["email"]
					});
					break;
				}
				case "otp_sent":
					if (this.interrupt?.kind === "otp") this.setInterrupt({
						...this.interrupt,
						sent: {
							channel: str(inner.channel),
							maskedDestination: str(inner.maskedDestination)
						},
						error: void 0
					});
					break;
				case "otp_verified":
					if (this.interrupt?.kind === "otp") this.setInterrupt(null);
					break;
				case "otp_invalid":
					if (this.interrupt?.kind === "otp") this.setInterrupt({
						...this.interrupt,
						error: str(inner.message) ?? "That code was incorrect.",
						attemptsRemaining: num(inner.attemptsRemaining)
					});
					break;
				case "write_confirmation_required":
					this.setInterrupt({
						kind: "confirm",
						toolId: str(inner.toolId),
						actionDescription: str(inner.actionDescription)
					});
					break;
				default: break;
			}
		}
		/** Tear down the underlying client. */
		disconnect() {
			this.client?.disconnect("widget closed");
			this.client = null;
			this.sessionId = null;
			this.activeRequestId = null;
			this.setInterrupt(null);
			this.setStatus("closed");
		}
	};
	//#endregion
	//#region src/logo.ts
	/**
	* The Smooth logo, inlined as an SVG string so the full-page header can render
	* it without a separate network fetch (the IIFE bundle is self-contained).
	*
	* GENERATED from `assets/smooth-logo.svg` — do not edit by hand. Regenerate with:
	*   node -e ...  (see the commit that added this file)
	*/
	const SMOOTH_LOGO_SVG = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<svg id=\"Layer_1\" data-name=\"Layer 1\" xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" viewBox=\"0 0 550 135\">\n  <defs>\n    <style>\n      .cls-1 {\n        fill: url(#linear-gradient-3);\n      }\n\n      .cls-2 {\n        fill: url(#linear-gradient-2);\n      }\n\n      .cls-3 {\n        fill: url(#linear-gradient);\n        fill-rule: evenodd;\n      }\n    </style>\n    <linearGradient id=\"linear-gradient\" x1=\"115.59\" y1=\"112.81\" x2=\"25.08\" y2=\"22.3\" gradientUnits=\"userSpaceOnUse\">\n      <stop offset=\".3\" stop-color=\"#f49f0a\"/>\n      <stop offset=\".79\" stop-color=\"#fb7a4d\"/>\n      <stop offset=\"1\" stop-color=\"#ff6b6c\"/>\n    </linearGradient>\n    <linearGradient id=\"linear-gradient-2\" x1=\"360.91\" y1=\"152.01\" x2=\"202.32\" y2=\"-6.59\" xlink:href=\"#linear-gradient\"/>\n    <linearGradient id=\"linear-gradient-3\" x1=\"443.91\" y1=\"30.15\" x2=\"531.36\" y2=\"117.59\" gradientUnits=\"userSpaceOnUse\">\n      <stop offset=\".43\" stop-color=\"#00a6a6\"/>\n      <stop offset=\"1\" stop-color=\"#1238dd\"/>\n    </linearGradient>\n  </defs>\n  <path class=\"cls-3\" d=\"M48.28,14.96c-12.39,5.21-22.54,14.64-28.65,26.61-6.12,11.97-7.8,25.72-4.77,38.81,3.04,13.09,10.6,24.69,21.36,32.75,10.76,8.06,24.02,12.05,37.44,11.28,13.42-.77,26.13-6.26,35.9-15.5,9.76-9.24,15.95-21.63,17.46-34.99,1.51-13.36-1.74-26.82-9.19-38.01-1.07-1.61-.64-3.78.97-4.85,1.61-1.07,3.78-.64,4.85.97,8.36,12.56,12.02,27.68,10.32,42.67-1.7,15-8.64,28.91-19.61,39.28-10.96,10.37-25.24,16.54-40.31,17.4-15.07.87-29.96-3.62-42.04-12.66-12.08-9.05-20.58-22.07-23.99-36.77-3.41-14.7-1.51-30.14,5.35-43.58,6.87-13.44,18.26-24.02,32.17-29.87,13.91-5.85,29.44-6.6,43.85-2.11,1.85.57,2.88,2.54,2.3,4.38-.57,1.85-2.54,2.88-4.38,2.3-12.83-4-26.67-3.33-39.06,1.88ZM111.39,19.75c0,2.07-1.68,3.75-3.75,3.75s-3.75-1.68-3.75-3.75,1.68-3.75,3.75-3.75,3.75,1.68,3.75,3.75ZM64.64,59.93c0,1.91,2.39,2.56,7.69,3.88,3.89.97,6.6,2.18,8.15,3.63,1.53,1.45,2.29,3.53,2.29,6.25,0,3.57-1.03,6.26-3.11,8.08-2.07,1.82-5.09,2.73-9.09,2.73h-9.6c-1.97,0-3.57-1.6-3.59-3.57-.01-1.99,1.6-3.61,3.59-3.61h9.41c3.15-.12,4.79-.95,4.91-2.47,0-1.3-1.03-2.21-3.07-2.73-6.91-1.72-11.11-3.44-12.6-5.15-1.48-1.71-2.23-3.77-2.23-6.19,0-6.59,3.2-9.85,9.59-9.8h10.77c1.99,0,3.6,1.61,3.6,3.59s-1.61,3.59-3.6,3.59h-9.69c-1.83,0-3.43.06-3.43,1.77Z\"/>\n  <path class=\"cls-2\" d=\"M205.52,48.44h-8.86c-.44-3.75-2.23-6.65-5.38-8.72-3.16-2.07-7.03-3.1-11.6-3.1h0c-3.35,0-6.27.54-8.78,1.62-2.49,1.09-4.44,2.59-5.84,4.48-1.39,1.89-2.08,4.05-2.08,6.46h0c0,2.01.49,3.75,1.46,5.2.97,1.44,2.22,2.63,3.74,3.58,1.53.95,3.13,1.72,4.8,2.32,1.68.6,3.22,1.09,4.62,1.46h0l7.68,2.06c1.97.52,4.17,1.23,6.6,2.14,2.43.92,4.75,2.16,6.98,3.72,2.23,1.56,4.07,3.56,5.52,6,1.45,2.44,2.18,5.43,2.18,8.98h0c0,4.08-1.07,7.77-3.2,11.08-2.12,3.29-5.22,5.91-9.3,7.86-4.08,1.95-9.02,2.92-14.82,2.92h0c-5.43,0-10.11-.87-14.06-2.62-3.95-1.75-7.05-4.19-9.3-7.32-2.25-3.12-3.53-6.75-3.84-10.88h9.46c.25,2.85,1.22,5.21,2.9,7.06,1.69,1.87,3.83,3.25,6.42,4.14,2.6.89,5.41,1.34,8.42,1.34h0c3.49,0,6.63-.57,9.4-1.72,2.79-1.13,4.99-2.73,6.62-4.8,1.63-2.05,2.44-4.46,2.44-7.22h0c0-2.51-.7-4.55-2.1-6.12-1.41-1.57-3.26-2.85-5.54-3.84-2.29-.99-4.77-1.85-7.44-2.58h0l-9.3-2.66c-5.91-1.71-10.59-4.13-14.04-7.28-3.44-3.16-5.16-7.29-5.16-12.38h0c0-4.23,1.15-7.93,3.46-11.1,2.29-3.16,5.39-5.62,9.3-7.38,3.91-1.76,8.27-2.64,13.08-2.64h0c4.88,0,9.21.87,13,2.6,3.8,1.73,6.81,4.11,9.04,7.12,2.23,3,3.4,6.41,3.52,10.22h0ZM229.16,105.18h-8.72v-56.74h8.42v8.86h.74c1.19-3.03,3.1-5.38,5.74-7.06,2.63-1.69,5.79-2.54,9.48-2.54h0c3.75,0,6.87.85,9.36,2.54,2.51,1.68,4.46,4.03,5.86,7.06h.58c1.45-2.92,3.63-5.25,6.54-7,2.91-1.73,6.39-2.6,10.46-2.6h0c5.07,0,9.21,1.58,12.44,4.74,3.23,3.17,4.84,8.09,4.84,14.76h0v37.98h-8.72v-37.98c0-4.19-1.14-7.18-3.42-8.98-2.29-1.79-4.99-2.68-8.1-2.68h0c-3.99,0-7.07,1.2-9.26,3.6-2.2,2.4-3.3,5.43-3.3,9.1h0v36.94h-8.86v-38.86c0-3.23-1.05-5.83-3.14-7.82-2.09-1.97-4.79-2.96-8.08-2.96h0c-2.27,0-4.38.6-6.34,1.8-1.96,1.21-3.53,2.88-4.72,5-1.2,2.13-1.8,4.59-1.8,7.38h0v35.46ZM333.9,106.36h0c-5.12,0-9.61-1.22-13.46-3.66-3.85-2.44-6.86-5.85-9.02-10.24-2.15-4.37-3.22-9.49-3.22-15.36h0c0-5.91,1.07-11.07,3.22-15.48,2.16-4.4,5.17-7.82,9.02-10.26,3.85-2.44,8.34-3.66,13.46-3.66h0c5.12,0,9.61,1.22,13.46,3.66,3.85,2.44,6.86,5.86,9.02,10.26,2.15,4.41,3.22,9.57,3.22,15.48h0c0,5.87-1.07,10.99-3.22,15.36-2.16,4.39-5.17,7.8-9.02,10.24-3.85,2.44-8.34,3.66-13.46,3.66ZM333.9,98.52h0c3.89,0,7.09-.99,9.6-2.98,2.52-2,4.38-4.63,5.58-7.88,1.21-3.25,1.82-6.77,1.82-10.56h0c0-3.79-.61-7.32-1.82-10.6-1.2-3.27-3.06-5.91-5.58-7.94-2.51-2.01-5.71-3.02-9.6-3.02h0c-3.89,0-7.09,1.01-9.6,3.02-2.51,2.03-4.37,4.67-5.58,7.94-1.2,3.28-1.8,6.81-1.8,10.6h0c0,3.79.6,7.31,1.8,10.56,1.21,3.25,3.07,5.88,5.58,7.88,2.51,1.99,5.71,2.98,9.6,2.98ZM395.94,106.36h0c-5.12,0-9.61-1.22-13.46-3.66-3.85-2.44-6.85-5.85-9-10.24-2.16-4.37-3.24-9.49-3.24-15.36h0c0-5.91,1.08-11.07,3.24-15.48,2.15-4.4,5.15-7.82,9-10.26,3.85-2.44,8.34-3.66,13.46-3.66h0c5.12,0,9.61,1.22,13.46,3.66,3.85,2.44,6.86,5.86,9.02,10.26,2.16,4.41,3.24,9.57,3.24,15.48h0c0,5.87-1.08,10.99-3.24,15.36-2.16,4.39-5.17,7.8-9.02,10.24-3.85,2.44-8.34,3.66-13.46,3.66ZM395.94,98.52h0c3.89,0,7.09-.99,9.6-2.98,2.52-2,4.38-4.63,5.58-7.88,1.21-3.25,1.82-6.77,1.82-10.56h0c0-3.79-.61-7.32-1.82-10.6-1.2-3.27-3.06-5.91-5.58-7.94-2.51-2.01-5.71-3.02-9.6-3.02h0c-3.88,0-7.08,1.01-9.6,3.02-2.51,2.03-4.37,4.67-5.58,7.94-1.2,3.28-1.8,6.81-1.8,10.6h0c0,3.79.6,7.31,1.8,10.56,1.21,3.25,3.07,5.88,5.58,7.88,2.52,1.99,5.72,2.98,9.6,2.98Z\"/>\n  <path class=\"cls-1\" d=\"M467.88,48.02v13.28h-35.79v-13.28h35.79ZM439.68,34.38h17.89v53.42c0,1.5.36,2.62,1.08,3.36.72.74,1.88,1.1,3.49,1.1.62,0,1.48-.07,2.59-.21,1.11-.14,1.91-.27,2.38-.41l2.31,13.02c-2.02.58-3.97.97-5.84,1.18-1.88.21-3.66.31-5.33.31-6.08,0-10.7-1.43-13.84-4.28-3.15-2.85-4.72-7.01-4.72-12.48v-55.01ZM506.59,72.63v32.71h-17.89V28.95h17.53v33.53h-1.13c1.4-4.55,3.6-8.21,6.59-11,2.99-2.79,7.01-4.18,12.07-4.18,4,0,7.48.89,10.46,2.67,2.97,1.78,5.28,4.29,6.92,7.54,1.64,3.25,2.46,7.02,2.46,11.33v36.5h-17.89v-33.02c0-3.21-.82-5.73-2.46-7.56-1.64-1.83-3.93-2.74-6.87-2.74-1.92,0-3.62.42-5.1,1.26-1.49.84-2.64,2.04-3.46,3.61-.82,1.57-1.23,3.49-1.23,5.74Z\"/>\n</svg>";
	//#endregion
	//#region src/styles.ts
	/**
	* Render the widget's scoped stylesheet — the "Aurora Glass" design system.
	*
	* Every brand value is injected as a CSS custom property on `:host` so a host
	* page can override colors per-instance and the rules below stay static. Two
	* extra tokens are *derived in CSS* from the brand vars so they adapt to any
	* theme (light or dark) without the caller supplying them:
	*
	*   --sac-primary-2  a darker shade of `primary`, used as the second stop of the
	*                    launcher / send / user-bubble gradients (depth without a
	*                    second brand input).
	*   --sac-surface-2  a faint wash derived from `text`, used for inset chrome
	*                    (composer field, close button, source cards). On a dark
	*                    panel it reads as a light overlay; on a light panel, dark.
	*
	* Deliberately framework-light: no Tailwind, no runtime CSS-in-JS — just a string
	* the web component drops into its shadow root. Modern color features
	* (`color-mix`) are used intentionally; the widget targets evergreen browsers.
	*
	* `mode` switches host positioning + panel sizing between the floating popover
	* (default) and the full-page layout (fills its container/viewport).
	*/
	function buildStyles(theme, mode = "popover") {
		return `
:host {
    --sac-text: ${theme.text};
    --sac-bg: ${theme.background};
    --sac-primary: ${theme.primary};
    --sac-primary-text: ${theme.primaryText};
    --sac-assistant-bubble: ${theme.assistantBubble};
    --sac-assistant-bubble-text: ${theme.assistantBubbleText};
    --sac-user-bubble: ${theme.userBubble};
    --sac-user-bubble-text: ${theme.userBubbleText};
    --sac-border: ${theme.border};

    /* Derived tokens — adapt to any brand color without a second input. */
    --sac-primary-2: color-mix(in srgb, var(--sac-primary) 78%, #000 22%);
    --sac-surface-2: color-mix(in srgb, var(--sac-text) 5%, transparent);
    --sac-radius: 22px;
    --sac-ease: cubic-bezier(.16, 1, .3, 1);

    ${mode === "fullpage" ? `/* Full-page: fill the host's box (sized by its container, else the viewport). */
    display: block;
    position: relative;
    width: 100%;
    height: 100%;
    min-height: 100vh;` : `/* Popover: float in the bottom-right corner. */
    position: fixed;
    bottom: 24px;
    right: 24px;
    z-index: 2147483000;`}
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif;
    -webkit-font-smoothing: antialiased;
}

* { box-sizing: border-box; }

/* ───────────────────────────── Launcher ───────────────────────────── */
.launcher {
    position: relative;
    width: 62px;
    height: 62px;
    border-radius: 50%;
    border: none;
    cursor: pointer;
    padding: 0;
    background: radial-gradient(120% 120% at 30% 20%,
        color-mix(in srgb, var(--sac-primary) 78%, #fff 22%) 0%,
        var(--sac-primary) 42%,
        var(--sac-primary-2) 130%);
    color: var(--sac-primary-text);
    display: flex;
    align-items: center;
    justify-content: center;
    box-shadow:
        0 1px 0 rgba(255, 255, 255, .25) inset,
        0 10px 24px -6px color-mix(in srgb, var(--sac-primary) 55%, transparent),
        0 18px 50px -12px rgba(0, 0, 0, .6);
    transition: transform .45s var(--sac-ease), box-shadow .45s var(--sac-ease), opacity .3s ease;
    isolation: isolate;
}
/* Breathing presence ring. */
.launcher::before {
    content: '';
    position: absolute;
    inset: -6px;
    border-radius: 50%;
    z-index: -1;
    background: radial-gradient(closest-side, color-mix(in srgb, var(--sac-primary) 45%, transparent), transparent 75%);
    animation: sac-breathe 3.4s ease-in-out infinite;
}
@keyframes sac-breathe { 0%, 100% { transform: scale(1); opacity: .55 } 50% { transform: scale(1.28); opacity: 0 } }
.launcher:hover {
    transform: translateY(-3px) scale(1.06);
    box-shadow:
        0 1px 0 rgba(255, 255, 255, .3) inset,
        0 16px 30px -6px color-mix(in srgb, var(--sac-primary) 60%, transparent),
        0 26px 60px -14px rgba(0, 0, 0, .7);
}
.launcher:active { transform: translateY(-1px) scale(.98); }
.launcher .ico { width: 27px; height: 27px; display: block; transition: transform .4s var(--sac-ease); }
.launcher:hover .ico { transform: rotate(-6deg) scale(1.04); }
.launcher.hidden { opacity: 0; transform: scale(.4) translateY(10px); pointer-events: none; }

/* ─────────────────────────────── Panel ────────────────────────────── */
.panel {
    width: 390px;
    max-width: calc(100vw - 40px);
    height: 600px;
    max-height: calc(100vh - 56px);
    display: flex;
    flex-direction: column;
    overflow: hidden;
    border-radius: var(--sac-radius);
    background: linear-gradient(180deg, color-mix(in srgb, var(--sac-bg) 92%, #fff 8%) 0%, var(--sac-bg) 22%);
    color: var(--sac-text);
    border: 1px solid color-mix(in srgb, var(--sac-border) 80%, transparent);
    box-shadow:
        0 0 0 1px rgba(255, 255, 255, .03) inset,
        0 40px 80px -24px rgba(0, 0, 0, .65),
        0 16px 40px -20px rgba(0, 0, 0, .5);
    transform-origin: bottom right;
    animation: sac-panel-in .5s var(--sac-ease) both;
    position: relative;
}
@keyframes sac-panel-in { from { opacity: 0; transform: translateY(16px) scale(.92) } to { opacity: 1; transform: none } }
.panel.hidden { display: none; }
/* Ambient brand glow bleeding from the top of the panel. */
.panel::before {
    content: '';
    position: absolute;
    left: 0; right: 0; top: 0;
    height: 140px;
    pointer-events: none;
    background: radial-gradient(120% 100% at 50% 0%, color-mix(in srgb, var(--sac-primary) 22%, transparent), transparent 70%);
}
/* Full-page: the panel becomes the whole surface. */
.panel.fullpage {
    width: 100%;
    height: 100%;
    min-height: 100vh;
    max-width: none;
    max-height: none;
    border: none;
    border-radius: 0;
    box-shadow: none;
    animation: none;
}

/* ─────────────────────────────── Header ───────────────────────────── */
.header {
    position: relative;
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 16px 16px 14px;
}
.avatar {
    width: 40px;
    height: 40px;
    border-radius: 13px;
    flex: none;
    background: linear-gradient(140deg, var(--sac-primary), var(--sac-primary-2));
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--sac-primary-text);
    box-shadow:
        0 6px 16px -6px color-mix(in srgb, var(--sac-primary) 60%, transparent),
        0 1px 0 rgba(255, 255, 255, .25) inset;
}
.avatar svg { width: 22px; height: 22px; }
.avatar .logo-wrap { display: flex; }
.avatar .logo { height: 22px; width: auto; display: block; }
.meta { min-width: 0; flex: 1; display: flex; flex-direction: column; gap: 2px; }
.title { font-weight: 650; font-size: 15.5px; letter-spacing: -.01em; line-height: 1.1; }
.status {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: color-mix(in srgb, var(--sac-text) 62%, transparent);
}
.dot {
    width: 7px; height: 7px;
    border-radius: 50%;
    flex: none;
    background: #34d399;
    color: #34d399;
    box-shadow: 0 0 0 0 rgba(52, 211, 153, .6);
    animation: sac-pulse 2.4s ease-out infinite;
}
.dot.connecting { background: #fbbf24; color: #fbbf24; animation: sac-pulse 1.1s ease-out infinite; }
.dot.error { background: #f87171; color: #f87171; animation: none; }
.dot.off { background: #94a3b8; color: #94a3b8; animation: none; }
@keyframes sac-pulse {
    0% { box-shadow: 0 0 0 0 color-mix(in srgb, currentColor 55%, transparent) }
    70% { box-shadow: 0 0 0 6px transparent }
    100% { box-shadow: 0 0 0 0 transparent }
}
.close {
    margin-left: auto;
    width: 32px; height: 32px;
    border-radius: 10px;
    border: none;
    cursor: pointer;
    background: var(--sac-surface-2);
    color: inherit;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: background .2s ease, transform .2s ease;
}
.close:hover { background: color-mix(in srgb, var(--sac-text) 12%, transparent); transform: translateY(1px); }
.close svg { width: 16px; height: 16px; opacity: .8; }
.powered { margin-left: auto; font-size: 10.5px; letter-spacing: .02em; opacity: .6; }
.header-sep { height: 1px; margin: 0 16px; background: linear-gradient(90deg, transparent, var(--sac-border), transparent); }

/* Full-page header: taller, logo-led, no close. */
.panel.fullpage .header { padding: 18px 22px; }
.panel.fullpage .avatar { width: 44px; height: 44px; }
.panel.fullpage .avatar .logo { height: 26px; }

/* ────────────────────────────── Messages ──────────────────────────── */
.messages {
    flex: 1;
    overflow-y: auto;
    padding: 18px 16px 8px;
    display: flex;
    flex-direction: column;
    gap: 12px;
    scroll-behavior: smooth;
}
.messages::-webkit-scrollbar { width: 8px; }
.messages::-webkit-scrollbar-thumb {
    background: color-mix(in srgb, var(--sac-text) 14%, transparent);
    border-radius: 99px;
    border: 2px solid transparent;
    background-clip: padding-box;
}
.messages::-webkit-scrollbar-thumb:hover {
    background: color-mix(in srgb, var(--sac-text) 24%, transparent);
    background-clip: padding-box;
}

.row {
    display: flex;
    gap: 9px;
    max-width: 88%;
    animation: sac-msg-in .42s var(--sac-ease) both;
}
@keyframes sac-msg-in { from { opacity: 0; transform: translateY(8px) } to { opacity: 1; transform: none } }
.row.user { align-self: flex-end; flex-direction: row-reverse; }
.row.assistant { align-self: flex-start; }
.mini {
    width: 26px; height: 26px;
    border-radius: 9px;
    flex: none;
    align-self: flex-end;
    background: linear-gradient(140deg, var(--sac-primary), var(--sac-primary-2));
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--sac-primary-text);
}
.mini svg { width: 15px; height: 15px; }

.bubble {
    padding: 11px 14px;
    border-radius: 16px;
    font-size: 14px;
    line-height: 1.5;
    white-space: pre-wrap;
    word-break: break-word;
    position: relative;
}
.bubble.assistant {
    background: linear-gradient(180deg, color-mix(in srgb, var(--sac-assistant-bubble) 86%, #fff 5%), var(--sac-assistant-bubble));
    color: var(--sac-assistant-bubble-text);
    border: 1px solid color-mix(in srgb, var(--sac-text) 8%, transparent);
    border-bottom-left-radius: 5px;
    box-shadow: 0 2px 8px -4px rgba(0, 0, 0, .4);
}
.bubble.user {
    background: linear-gradient(165deg,
        color-mix(in srgb, var(--sac-user-bubble) 88%, #fff 12%),
        var(--sac-user-bubble) 60%,
        color-mix(in srgb, var(--sac-user-bubble) 80%, var(--sac-primary-2) 20%));
    color: var(--sac-user-bubble-text);
    border-bottom-right-radius: 5px;
    box-shadow: 0 6px 16px -8px color-mix(in srgb, var(--sac-primary) 50%, transparent);
}
.bubble.greeting {
    background: transparent;
    border: 1px dashed color-mix(in srgb, var(--sac-text) 14%, transparent);
    color: color-mix(in srgb, var(--sac-text) 80%, transparent);
    box-shadow: none;
}

/* Typing indicator (assistant bubble with no text yet). */
.bubble.typing { display: flex; gap: 4px; padding: 14px 15px; }
.bubble.typing i {
    width: 7px; height: 7px;
    border-radius: 50%;
    background: color-mix(in srgb, var(--sac-assistant-bubble-text) 55%, transparent);
    animation: sac-typing 1.3s ease-in-out infinite;
}
.bubble.typing i:nth-child(2) { animation-delay: .18s; }
.bubble.typing i:nth-child(3) { animation-delay: .36s; }
@keyframes sac-typing { 0%, 60%, 100% { transform: translateY(0); opacity: .4 } 30% { transform: translateY(-5px); opacity: 1 } }

.cursor::after {
    content: '';
    display: inline-block;
    width: 2px; height: 1.05em;
    margin-left: 2px;
    vertical-align: -2px;
    border-radius: 2px;
    background: currentColor;
    animation: sac-blink 1s steps(2, start) infinite;
}
@keyframes sac-blink { to { opacity: 0 } }

/* Full-page: center the conversation in a readable column. */
.panel.fullpage .messages { padding: 26px 20px; }
.panel.fullpage .row { max-width: 760px; width: 100%; margin-left: auto; margin-right: auto; }
.panel.fullpage .row.user { max-width: 80%; margin-right: 0; }

/* ───────────────── Sources (grounding citations) ──────────────────── */
.sources {
    align-self: flex-start;
    max-width: 88%;
    margin: -4px 0 0 35px;
}
.panel.fullpage .sources { max-width: 760px; width: 100%; margin-left: auto; margin-right: auto; }
.sources summary {
    cursor: pointer;
    list-style: none;
    display: inline-flex;
    align-items: center;
    gap: 7px;
    font-size: 12px;
    font-weight: 600;
    color: color-mix(in srgb, var(--sac-text) 70%, transparent);
    padding: 5px 0;
    user-select: none;
}
.sources summary::-webkit-details-marker { display: none; }
.sources .chev { transition: transform .2s var(--sac-ease); flex: none; }
.sources details[open] .chev { transform: rotate(90deg); }
.sources .count {
    background: color-mix(in srgb, var(--sac-primary) 18%, transparent);
    color: color-mix(in srgb, var(--sac-primary) 92%, #fff);
    font-size: 10.5px;
    font-weight: 700;
    padding: 1px 7px;
    border-radius: 99px;
}
.sources ol { list-style: none; margin: 6px 0 2px; padding: 0; display: flex; flex-direction: column; gap: 7px; }
.sources li {
    background: var(--sac-surface-2);
    border: 1px solid color-mix(in srgb, var(--sac-border) 70%, transparent);
    border-left: 2px solid var(--sac-primary);
    border-radius: 9px;
    padding: 8px 10px;
}
.sources .src-title {
    color: color-mix(in srgb, var(--sac-primary) 92%, #fff);
    font-weight: 600;
    font-size: 12.5px;
    text-decoration: none;
    word-break: break-word;
}
.sources a.src-title:hover { text-decoration: underline; }
.sources span.src-title { color: var(--sac-text); opacity: .95; }
.sources .src-snippet {
    display: block;
    margin-top: 3px;
    font-size: 11.5px;
    line-height: 1.45;
    color: color-mix(in srgb, var(--sac-text) 55%, transparent);
    white-space: normal;
}

/* ────────────────────────────── Composer ──────────────────────────── */
.composer-wrap { padding: 12px 14px calc(12px + env(safe-area-inset-bottom)); }
.composer {
    display: flex;
    align-items: flex-end;
    gap: 8px;
    padding: 7px 7px 7px 14px;
    border-radius: 18px;
    background: var(--sac-surface-2);
    border: 1px solid color-mix(in srgb, var(--sac-border) 80%, transparent);
    transition: border-color .25s ease, box-shadow .25s ease, background .25s ease;
}
.composer:focus-within {
    border-color: color-mix(in srgb, var(--sac-primary) 60%, transparent);
    box-shadow: 0 0 0 4px color-mix(in srgb, var(--sac-primary) 14%, transparent);
}
.composer textarea {
    flex: 1;
    resize: none;
    border: none;
    background: transparent;
    color: var(--sac-text);
    font-family: inherit;
    font-size: 14px;
    line-height: 1.45;
    max-height: 120px;
    padding: 6px 0;
    outline: none;
}
.composer textarea::placeholder { color: color-mix(in srgb, var(--sac-text) 42%, transparent); }
.send {
    width: 38px; height: 38px;
    flex: none;
    border: none;
    border-radius: 13px;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    background: linear-gradient(150deg, var(--sac-primary), var(--sac-primary-2));
    color: var(--sac-primary-text);
    box-shadow:
        0 6px 14px -6px color-mix(in srgb, var(--sac-primary) 65%, transparent),
        0 1px 0 rgba(255, 255, 255, .25) inset;
    transition: transform .2s var(--sac-ease), box-shadow .2s var(--sac-ease), opacity .2s ease;
}
.send svg { width: 18px; height: 18px; }
.send:hover { transform: translateY(-1px) scale(1.05); }
.send:active { transform: scale(.94); }
.send:disabled { opacity: .4; cursor: default; transform: none; box-shadow: none; }
.footer {
    text-align: center;
    margin-top: 9px;
    font-size: 10.5px;
    letter-spacing: .04em;
    color: color-mix(in srgb, var(--sac-text) 38%, transparent);
}
.footer b { font-weight: 600; color: color-mix(in srgb, var(--sac-text) 55%, transparent); }

/* ─────────────────── Pre-chat identity form ───────────────────────── */
.prechat { flex: 1; display: flex; flex-direction: column; justify-content: center; gap: 18px; padding: 22px 20px; }
.pc-head { text-align: center; }
.pc-title { font-size: 17px; font-weight: 650; letter-spacing: -.01em; }
.pc-sub { margin-top: 4px; font-size: 13px; color: color-mix(in srgb, var(--sac-text) 60%, transparent); }
.pc-form { display: flex; flex-direction: column; gap: 12px; }
.pc-field { display: flex; flex-direction: column; gap: 5px; }
.pc-field span { font-size: 12px; font-weight: 600; color: color-mix(in srgb, var(--sac-text) 70%, transparent); }
.pc-field input {
    border: 1px solid color-mix(in srgb, var(--sac-border) 80%, transparent);
    background: var(--sac-surface-2);
    color: var(--sac-text);
    border-radius: 12px;
    padding: 11px 13px;
    font-family: inherit;
    font-size: 14px;
    outline: none;
    transition: border-color .2s ease, box-shadow .2s ease;
}
.pc-field input::placeholder { color: color-mix(in srgb, var(--sac-text) 42%, transparent); }
.pc-field input:focus {
    border-color: color-mix(in srgb, var(--sac-primary) 60%, transparent);
    box-shadow: 0 0 0 4px color-mix(in srgb, var(--sac-primary) 14%, transparent);
}
.pc-submit {
    margin-top: 4px;
    border: none;
    border-radius: 13px;
    padding: 12px;
    cursor: pointer;
    background: linear-gradient(150deg, var(--sac-primary), var(--sac-primary-2));
    color: var(--sac-primary-text);
    font-weight: 650;
    font-size: 14px;
    box-shadow: 0 6px 14px -6px color-mix(in srgb, var(--sac-primary) 65%, transparent), 0 1px 0 rgba(255, 255, 255, .25) inset;
    transition: transform .2s var(--sac-ease);
}
.pc-submit:hover { transform: translateY(-1px); }
.pc-submit:active { transform: scale(.98); }

/* ─────────────────── Starter-prompt chips ─────────────────────────── */
.prompts { display: flex; flex-wrap: wrap; gap: 8px; margin: 2px 0 2px 35px; }
.panel.fullpage .prompts { margin-left: auto; margin-right: auto; max-width: 760px; width: 100%; }
.chip {
    border: 1px solid color-mix(in srgb, var(--sac-border) 80%, transparent);
    background: var(--sac-surface-2);
    color: var(--sac-text);
    border-radius: 999px;
    padding: 8px 13px;
    font-family: inherit;
    font-size: 12.5px;
    cursor: pointer;
    text-align: left;
    transition: border-color .2s ease, background .2s ease, transform .2s ease;
}
.chip:hover {
    border-color: color-mix(in srgb, var(--sac-primary) 50%, transparent);
    background: color-mix(in srgb, var(--sac-primary) 10%, var(--sac-surface-2));
    transform: translateY(-1px);
}

/* ─────────────── OTP / tool-confirmation interrupt ────────────────── */
.interrupt { padding: 0 14px; }
.int-card {
    border: 1px solid color-mix(in srgb, var(--sac-primary) 35%, var(--sac-border));
    background: color-mix(in srgb, var(--sac-primary) 8%, var(--sac-surface-2));
    border-radius: 14px;
    padding: 12px 13px;
    animation: sac-msg-in .3s var(--sac-ease) both;
}
.int-head { display: flex; align-items: center; gap: 8px; }
.int-ico { display: flex; color: var(--sac-primary); }
.int-ico svg { width: 17px; height: 17px; }
.int-title { font-size: 13.5px; font-weight: 650; }
.int-desc { margin-top: 5px; font-size: 12.5px; line-height: 1.45; color: color-mix(in srgb, var(--sac-text) 80%, transparent); }
.int-sent { margin-top: 6px; font-size: 11.5px; color: color-mix(in srgb, var(--sac-text) 60%, transparent); }
.int-row { display: flex; gap: 8px; margin-top: 10px; }
.int-input {
    flex: 1;
    min-width: 0;
    border: 1px solid color-mix(in srgb, var(--sac-border) 80%, transparent);
    background: var(--sac-bg);
    color: var(--sac-text);
    border-radius: 10px;
    padding: 9px 11px;
    font-family: inherit;
    font-size: 14px;
    letter-spacing: .14em;
    outline: none;
    transition: border-color .2s ease, box-shadow .2s ease;
}
.int-input:focus {
    border-color: color-mix(in srgb, var(--sac-primary) 60%, transparent);
    box-shadow: 0 0 0 4px color-mix(in srgb, var(--sac-primary) 14%, transparent);
}
.int-btn {
    border: 1px solid color-mix(in srgb, var(--sac-border) 80%, transparent);
    background: var(--sac-surface-2);
    color: var(--sac-text);
    border-radius: 10px;
    padding: 9px 14px;
    font-family: inherit;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
    transition: transform .2s var(--sac-ease), background .2s ease, border-color .2s ease;
}
.int-btn:hover { transform: translateY(-1px); }
.int-btn.primary {
    border: none;
    background: linear-gradient(150deg, var(--sac-primary), var(--sac-primary-2));
    color: var(--sac-primary-text);
    box-shadow: 0 6px 14px -6px color-mix(in srgb, var(--sac-primary) 65%, transparent);
}
.int-row .int-btn { flex: 1; }
.int-row .int-input + .int-btn { flex: 0 0 auto; }
.int-error { margin-top: 8px; font-size: 12px; color: #f87171; }

.hidden { display: none !important; }

@media (prefers-reduced-motion: reduce) {
    .launcher::before, .dot, .bubble.typing i { animation: none !important; }
    .panel, .row, .launcher, .send, .close { animation: none !important; transition: none !important; }
}
`;
	}
	//#endregion
	//#region src/element.ts
	const ELEMENT_TAG = "smooth-agent-chat";
	const OBSERVED = [
		"endpoint",
		"agent-id",
		"agent-name",
		"placeholder",
		"greeting",
		"start-open",
		"mode"
	];
	/**
	* Inline SVG icons (static, trusted strings — never interpolated with user data).
	* Kept here so the IIFE bundle is self-contained: no icon-font or network fetch.
	*/
	const ICON = {
		/** Launcher — a speech bubble carrying a spark (chat + AI). */
		spark: `<svg class="ico" viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg"><path d="M12 3.5c-4.7 0-8.5 3.2-8.5 7.2 0 2.2 1.2 4.2 3 5.5v3.3l3.2-1.7c.7.1 1.5.2 2.3.2 4.7 0 8.5-3.2 8.5-7.3S16.7 3.5 12 3.5Z" fill="currentColor" opacity=".22"/><path d="M13.4 7.2 9 12.6h2.6l-1 4.2 4.4-5.4h-2.6l1-4.2Z" fill="currentColor"/></svg>`,
		/** Small assistant avatar used beside each assistant message. */
		bot: `<svg viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg"><rect x="4.5" y="7.5" width="15" height="11" rx="3.5" stroke="currentColor" stroke-width="1.6"/><path d="M12 4.5v3M8.5 12.2h.01M15.5 12.2h.01" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/><path d="M9.5 15.4c.7.6 1.5.9 2.5.9s1.8-.3 2.5-.9" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></svg>`,
		/** Close (collapse panel) — a downward chevron. */
		close: `<svg viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg"><path d="m7 10 5 5 5-5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
		/** Send — an upward arrow. */
		send: `<svg viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg"><path d="M12 19V6M12 6l-5.5 5.5M12 6l5.5 5.5" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
		/** Sources disclosure caret. */
		chev: `<svg width="11" height="11" viewBox="0 0 24 24" fill="none"><path d="m9 6 6 6-6 6" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
		/** OTP interrupt — a padlock. */
		lock: `<svg viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg"><rect x="5" y="10.5" width="14" height="9.5" rx="2.2" stroke="currentColor" stroke-width="1.7"/><path d="M8 10.5V8a4 4 0 0 1 8 0v2.5" stroke="currentColor" stroke-width="1.7"/></svg>`,
		/** Tool-confirmation interrupt — a shield. */
		shield: `<svg viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg"><path d="M12 3 5 6v5c0 4.4 3 7.2 7 8.5 4-1.3 7-4.1 7-8.5V6l-7-3Z" stroke="currentColor" stroke-width="1.7" stroke-linejoin="round"/><path d="m9 11.5 2 2 4-4" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/></svg>`
	};
	/**
	* Return `url` only if it is a valid absolute `http(s)` URL, else `null`.
	*
	* SECURITY: citation URLs originate from indexed content (web / GitHub
	* connectors), which can be attacker-influenceable. Assigning an arbitrary
	* string to `<a>.href` allows `javascript:`/`data:`/`vbscript:` URLs that
	* execute on click — a stored-XSS vector. Only http(s) links are rendered as
	* anchors; anything else falls back to plain text.
	*/
	function safeHttpUrl(url) {
		if (!url) return null;
		try {
			const parsed = new URL(url);
			return parsed.protocol === "http:" || parsed.protocol === "https:" ? parsed.href : null;
		} catch {
			return null;
		}
	}
	var SmoothAgentChatElement = class extends HTMLElement {
		static get observedAttributes() {
			return OBSERVED;
		}
		root;
		controller = null;
		overrides = {};
		open = false;
		messages = [];
		status = "idle";
		mounted = false;
		/** True once the visitor has cleared the pre-chat identity gate (or it's not needed). */
		userInfoSatisfied = false;
		/** True after the visitor has sent their first message (hides starter chips). */
		hasSent = false;
		/** Starter prompts shown as chips in the empty state. */
		examplePrompts = [];
		/** Current mid-turn interrupt (OTP / tool-confirmation), or null. */
		interrupt = null;
		interruptEl = null;
		panelEl = null;
		launcherEl = null;
		messagesEl = null;
		statusEl = null;
		dotEl = null;
		inputEl = null;
		sendBtn = null;
		constructor() {
			super();
			this.root = this.attachShadow({ mode: "open" });
		}
		connectedCallback() {
			this.mounted = true;
			this.render();
		}
		disconnectedCallback() {
			this.mounted = false;
			this.controller?.disconnect();
			this.controller = null;
		}
		attributeChangedCallback() {
			if (this.mounted) this.render();
		}
		/**
		* Programmatically merge config overrides (endpoint, agentId, theme, …). Values
		* set here take precedence over HTML attributes. Re-renders the widget.
		*/
		configure(config) {
			this.overrides = {
				...this.overrides,
				...config
			};
			if (config.theme) this.overrides.theme = {
				...this.overrides.theme ?? {},
				...config.theme
			};
			if (this.mounted) this.render();
		}
		/** Open the chat panel. */
		openChat() {
			this.open = true;
			this.syncOpenState();
			this.controller?.connect().catch(() => {});
		}
		/** Collapse the chat panel back to the launcher. */
		closeChat() {
			this.open = false;
			this.syncOpenState();
		}
		readConfig() {
			const endpoint = this.overrides.endpoint ?? this.getAttribute("endpoint") ?? "";
			const agentId = this.overrides.agentId ?? this.getAttribute("agent-id") ?? "";
			if (!endpoint || !agentId) return null;
			const theme = this.overrides.theme;
			const modeAttr = this.getAttribute("mode");
			return {
				endpoint,
				mode: this.overrides.mode ?? (modeAttr === "fullpage" ? "fullpage" : modeAttr === "popover" ? "popover" : void 0) ?? "popover",
				agentId,
				agentName: this.overrides.agentName ?? this.getAttribute("agent-name") ?? void 0,
				userName: this.overrides.userName,
				userEmail: this.overrides.userEmail,
				userPhone: this.overrides.userPhone,
				placeholder: this.overrides.placeholder ?? this.getAttribute("placeholder") ?? void 0,
				greeting: this.overrides.greeting ?? this.getAttribute("greeting") ?? void 0,
				connectionErrorMessage: this.overrides.connectionErrorMessage,
				startOpen: this.overrides.startOpen ?? this.hasAttribute("start-open"),
				examplePrompts: this.overrides.examplePrompts,
				requireName: this.overrides.requireName,
				requireEmail: this.overrides.requireEmail,
				requirePhone: this.overrides.requirePhone,
				allowAnonymous: this.overrides.allowAnonymous,
				theme
			};
		}
		render() {
			const config = this.readConfig();
			if (!config) {
				this.root.innerHTML = "";
				return;
			}
			const resolved = resolveConfig(config);
			if (!this.controller) {
				this.controller = new ConversationController(config, {
					onMessages: (messages) => {
						this.messages = messages;
						this.renderMessages(resolved.greeting);
					},
					onStatus: (status) => {
						this.status = status;
						this.renderStatus();
						this.renderComposerState();
					},
					onInterrupt: (interrupt) => {
						this.interrupt = interrupt;
						this.renderInterrupt();
					}
				});
				if (resolved.startOpen) this.open = true;
			}
			const fullpage = resolved.mode === "fullpage";
			if (fullpage) this.open = true;
			const style = document.createElement("style");
			style.textContent = buildStyles(resolved.theme, resolved.mode);
			const monogram = escapeHtml((resolved.agentName.trim().charAt(0) || "A").toUpperCase());
			const header = fullpage ? `<div class="header">
                    <div class="avatar"><span class="logo-wrap">${SMOOTH_LOGO_SVG}</span></div>
                    <div class="meta">
                        <span class="title">${escapeHtml(resolved.agentName)}</span>
                        <span class="status"><span class="dot off"></span><span class="status-text"></span></span>
                    </div>
                    <span class="powered">powered by smooth-operator</span>
                </div>` : `<div class="header">
                    <div class="avatar">${monogram}</div>
                    <div class="meta">
                        <span class="title">${escapeHtml(resolved.agentName)}</span>
                        <span class="status"><span class="dot off"></span><span class="status-text"></span></span>
                    </div>
                    <button class="close" aria-label="Close chat">${ICON.close}</button>
                </div>`;
			this.examplePrompts = resolved.examplePrompts;
			const gating = needsUserInfo(resolved) && !this.userInfoSatisfied;
			const field = (name, type, label, autocomplete) => `<label class="pc-field"><span>${escapeHtml(label)}</span><input name="${name}" type="${type}" autocomplete="${autocomplete}" required /></label>`;
			const prechatHtml = `
            <div class="prechat">
                <div class="pc-head">
                    <div class="pc-title">Before we chat</div>
                    <div class="pc-sub">A couple details so ${escapeHtml(resolved.agentName)} can help.</div>
                </div>
                <form class="pc-form" novalidate>
                    ${resolved.requireName ? field("name", "text", "Name", "name") : ""}
                    ${resolved.requireEmail ? field("email", "email", "Email", "email") : ""}
                    ${resolved.requirePhone ? field("phone", "tel", "Phone", "tel") : ""}
                    <button type="submit" class="pc-submit">Start chat</button>
                </form>
            </div>`;
			const chatHtml = `
                <div class="messages"></div>
                <div class="interrupt hidden"></div>
                <div class="composer-wrap">
                    <div class="composer">
                        <textarea rows="1" placeholder="${escapeHtml(resolved.placeholder)}"></textarea>
                        <button class="send" type="button" aria-label="Send message">${ICON.send}</button>
                    </div>
                    <div class="footer">powered by <b>smooth&#8209;operator</b></div>
                </div>`;
			const container = document.createElement("div");
			container.innerHTML = `
            ${fullpage ? "" : `<button class="launcher" part="launcher" aria-label="Open chat">${ICON.spark}</button>`}
            <div class="panel${fullpage ? " fullpage" : " hidden"}" part="panel" role="${fullpage ? "region" : "dialog"}" aria-label="${escapeHtml(resolved.agentName)} chat">
                ${header}
                <div class="header-sep"></div>
                ${gating ? prechatHtml : chatHtml}
            </div>
        `;
			const logoSvg = container.querySelector(".logo-wrap svg");
			if (logoSvg) logoSvg.setAttribute("class", "logo");
			this.root.replaceChildren(style, container);
			this.launcherEl = container.querySelector(".launcher");
			this.panelEl = container.querySelector(".panel");
			this.messagesEl = container.querySelector(".messages");
			this.statusEl = container.querySelector(".status-text");
			this.dotEl = container.querySelector(".dot");
			this.inputEl = container.querySelector("textarea");
			this.sendBtn = container.querySelector(".send");
			this.interruptEl = container.querySelector(".interrupt");
			this.launcherEl?.addEventListener("click", () => this.openChat());
			container.querySelector(".close")?.addEventListener("click", () => this.closeChat());
			this.sendBtn?.addEventListener("click", () => this.submit());
			this.inputEl?.addEventListener("input", () => this.autosize());
			this.inputEl?.addEventListener("keydown", (ev) => {
				if (ev.key === "Enter" && !ev.shiftKey) {
					ev.preventDefault();
					this.submit();
				}
			});
			const pcForm = container.querySelector(".pc-form");
			pcForm?.addEventListener("submit", (ev) => {
				ev.preventDefault();
				this.handlePrechatSubmit(pcForm);
			});
			if (fullpage && !gating) this.controller?.connect().catch(() => {});
			this.syncOpenState();
			if (!gating) this.renderMessages(resolved.greeting);
			this.renderStatus();
			this.renderComposerState();
			this.renderInterrupt();
		}
		/**
		* Render (or clear) the mid-turn interrupt overlay above the composer:
		* an OTP code prompt or a tool-write confirmation. Server-supplied text is
		* set via `textContent` (never innerHTML); only static icons use innerHTML.
		*/
		renderInterrupt() {
			const el = this.interruptEl;
			if (!el) return;
			el.replaceChildren();
			const it = this.interrupt;
			if (!it) {
				el.classList.add("hidden");
				return;
			}
			el.classList.remove("hidden");
			const card = document.createElement("div");
			card.className = "int-card";
			const head = document.createElement("div");
			head.className = "int-head";
			const ico = document.createElement("span");
			ico.className = "int-ico";
			ico.innerHTML = it.kind === "otp" ? ICON.lock : ICON.shield;
			const title = document.createElement("span");
			title.className = "int-title";
			title.textContent = it.kind === "otp" ? "Verification required" : "Confirm this action";
			head.append(ico, title);
			card.appendChild(head);
			if (it.actionDescription) {
				const desc = document.createElement("div");
				desc.className = "int-desc";
				desc.textContent = it.actionDescription;
				card.appendChild(desc);
			}
			if (it.kind === "otp") {
				if (it.sent?.maskedDestination) {
					const sent = document.createElement("div");
					sent.className = "int-sent";
					sent.textContent = `Code sent to ${it.sent.maskedDestination}${it.sent.channel ? ` via ${it.sent.channel}` : ""}.`;
					card.appendChild(sent);
				}
				const row = document.createElement("div");
				row.className = "int-row";
				const input = document.createElement("input");
				input.className = "int-input";
				input.type = "text";
				input.inputMode = "numeric";
				input.autocomplete = "one-time-code";
				input.placeholder = "Enter code";
				const submit = () => {
					const code = input.value.trim();
					if (code) this.controller?.verifyOtp(code);
				};
				input.addEventListener("keydown", (ev) => {
					if (ev.key === "Enter") {
						ev.preventDefault();
						submit();
					}
				});
				const verify = document.createElement("button");
				verify.className = "int-btn primary";
				verify.type = "button";
				verify.textContent = "Verify";
				verify.addEventListener("click", submit);
				row.append(input, verify);
				card.appendChild(row);
				if (it.error) {
					const err = document.createElement("div");
					err.className = "int-error";
					err.textContent = it.attemptsRemaining != null ? `${it.error} (${it.attemptsRemaining} left)` : it.error;
					card.appendChild(err);
				}
				queueMicrotask(() => input.focus());
			} else {
				const row = document.createElement("div");
				row.className = "int-row";
				const decline = document.createElement("button");
				decline.className = "int-btn";
				decline.type = "button";
				decline.textContent = "Decline";
				decline.addEventListener("click", () => this.controller?.confirmTool(false));
				const approve = document.createElement("button");
				approve.className = "int-btn primary";
				approve.type = "button";
				approve.textContent = "Approve";
				approve.addEventListener("click", () => this.controller?.confirmTool(true));
				row.append(decline, approve);
				card.appendChild(row);
			}
			el.appendChild(card);
		}
		/** Collect identity from the pre-chat form, then drop into the chat view. */
		handlePrechatSubmit(form) {
			if (!form.reportValidity()) return;
			const data = new FormData(form);
			const val = (k) => data.get(k)?.trim() || void 0;
			this.controller?.setUserInfo({
				name: val("name"),
				email: val("email"),
				phone: val("phone")
			});
			this.userInfoSatisfied = true;
			this.render();
			this.controller?.connect().catch(() => {});
		}
		/** Send a starter prompt (from a chip click). */
		submitPrompt(text) {
			if (!this.inputEl) return;
			this.inputEl.value = text;
			this.submit();
		}
		syncOpenState() {
			if (this.panelEl?.classList.contains("fullpage")) {
				this.inputEl?.focus();
				return;
			}
			this.panelEl?.classList.toggle("hidden", !this.open);
			this.launcherEl?.classList.toggle("hidden", this.open);
			if (this.open) this.inputEl?.focus();
		}
		/** Grow the textarea with its content, up to the CSS max-height. */
		autosize() {
			const ta = this.inputEl;
			if (!ta) return;
			ta.style.height = "auto";
			ta.style.height = `${ta.scrollHeight}px`;
		}
		renderMessages(greeting) {
			if (!this.messagesEl) return;
			this.messagesEl.replaceChildren();
			if (this.messages.length === 0 && greeting) this.messagesEl.appendChild(this.buildRow("assistant", this.greetingBubble(greeting)));
			if (!this.hasSent && this.messages.length === 0 && this.examplePrompts.length > 0) {
				const chips = document.createElement("div");
				chips.className = "prompts";
				for (const prompt of this.examplePrompts) {
					const chip = document.createElement("button");
					chip.type = "button";
					chip.className = "chip";
					chip.textContent = prompt;
					chip.addEventListener("click", () => this.submitPrompt(prompt));
					chips.appendChild(chip);
				}
				this.messagesEl.appendChild(chips);
			}
			for (const msg of this.messages) {
				const bubble = document.createElement("div");
				bubble.className = `bubble ${msg.role}`;
				if (msg.role === "assistant" && msg.streaming && !msg.text) {
					bubble.classList.add("typing");
					bubble.append(this.typingDot(), this.typingDot(), this.typingDot());
				} else if (msg.streaming) {
					bubble.classList.add("cursor");
					bubble.textContent = msg.text;
				} else bubble.textContent = msg.text;
				this.messagesEl.appendChild(this.buildRow(msg.role, bubble));
				if (msg.role === "assistant" && !msg.streaming && msg.citations && msg.citations.length > 0) this.messagesEl.appendChild(this.renderSources(msg.citations));
			}
			this.messagesEl.scrollTop = this.messagesEl.scrollHeight;
		}
		/** Wrap a bubble in a `.row`, prefixing assistant rows with a mini avatar. */
		buildRow(role, bubble) {
			const row = document.createElement("div");
			row.className = `row ${role}`;
			if (role === "assistant") {
				const mini = document.createElement("div");
				mini.className = "mini";
				mini.innerHTML = ICON.bot;
				row.appendChild(mini);
			}
			row.appendChild(bubble);
			return row;
		}
		greetingBubble(greeting) {
			const b = document.createElement("div");
			b.className = "bubble assistant greeting";
			b.textContent = greeting;
			return b;
		}
		typingDot() {
			return document.createElement("i");
		}
		/**
		* Build the collapsible "Sources (N)" block for an assistant message's
		* citations. Title/snippet are set via `textContent` (never innerHTML) so
		* citation text can't inject markup; only the static chevron + numeric count
		* use innerHTML.
		*/
		renderSources(citations) {
			const wrap = document.createElement("div");
			wrap.className = "sources";
			wrap.setAttribute("part", "sources");
			const details = document.createElement("details");
			details.open = true;
			const summary = document.createElement("summary");
			const chev = document.createElement("span");
			chev.className = "chev";
			chev.innerHTML = ICON.chev;
			const label = document.createElement("span");
			label.textContent = "Sources";
			const count = document.createElement("span");
			count.className = "count";
			count.textContent = String(citations.length);
			summary.append(chev, label, count);
			details.appendChild(summary);
			const list = document.createElement("ol");
			for (const c of citations) {
				const li = document.createElement("li");
				let titleEl;
				const safeUrl = safeHttpUrl(c.url);
				if (safeUrl) {
					const a = document.createElement("a");
					a.className = "src-title";
					a.href = safeUrl;
					a.target = "_blank";
					a.rel = "noopener noreferrer";
					titleEl = a;
				} else {
					titleEl = document.createElement("span");
					titleEl.className = "src-title";
				}
				titleEl.textContent = c.title || c.id || "Source";
				li.appendChild(titleEl);
				if (c.snippet) {
					const snip = document.createElement("span");
					snip.className = "src-snippet";
					snip.textContent = c.snippet;
					li.appendChild(snip);
				}
				list.appendChild(li);
			}
			details.appendChild(list);
			wrap.appendChild(details);
			return wrap;
		}
		renderStatus() {
			const label = {
				idle: "",
				connecting: "Connecting…",
				ready: "Online",
				error: "Connection issue",
				closed: "Disconnected"
			};
			if (this.statusEl) this.statusEl.textContent = label[this.status];
			if (this.dotEl) {
				const mod = this.status === "ready" ? "" : this.status === "connecting" ? " connecting" : this.status === "error" ? " error" : " off";
				this.dotEl.className = `dot${mod}`;
			}
		}
		renderComposerState() {
			const busy = this.status === "connecting";
			if (this.sendBtn) this.sendBtn.disabled = busy;
			if (this.inputEl) this.inputEl.disabled = busy;
		}
		submit() {
			if (!this.inputEl || !this.controller) return;
			const text = this.inputEl.value;
			if (!text.trim()) return;
			this.inputEl.value = "";
			this.hasSent = true;
			this.autosize();
			this.controller.send(text);
		}
	};
	function escapeHtml(value) {
		return value.replace(/[&<>"']/g, (c) => {
			switch (c) {
				case "&": return "&amp;";
				case "<": return "&lt;";
				case ">": return "&gt;";
				case "\"": return "&quot;";
				default: return "&#39;";
			}
		});
	}
	/** Register the custom element once. Safe to call multiple times. */
	function defineChatWidget() {
		if (typeof customElements !== "undefined" && !customElements.get("smooth-agent-chat")) customElements.define(ELEMENT_TAG, SmoothAgentChatElement);
	}
	/**
	* Programmatically create, configure, and append a widget to the page.
	* Returns the element so the host can drive `openChat()` / `closeChat()`.
	*/
	function mountChatWidget(config, target = document.body) {
		defineChatWidget();
		const el = document.createElement(ELEMENT_TAG);
		el.configure(config);
		target.appendChild(el);
		return el;
	}
	/**
	* Ergonomic helper for the full-page layout: mounts a `<smooth-agent-chat>` in
	* `mode: "fullpage"` (no launcher — the chat fills its container/viewport with a
	* Smooth-branded header, a scrollable message list, and an input bar) and
	* returns the element.
	*
	* `target` defaults to `document.body`; pass a sized container to embed the
	* full-page chat inside a layout region (e.g. a `/chat` route shell or an
	* iframe). The `mode` is forced to `"fullpage"` regardless of the passed config.
	*
	* ```ts
	* mountFullPageChat({ endpoint: 'wss://…/ws', agentId: '…', agentName: 'Support' });
	* ```
	*/
	function mountFullPageChat(config, target = document.body) {
		return mountChatWidget({
			...config,
			mode: "fullpage"
		}, target);
	}
	//#endregion
	//#region src/standalone.ts
	defineChatWidget();
	/** Convenience alias matching the global API surface (`SmoothAgentChat.mount`). */
	function mount(config, target) {
		return mountChatWidget(config, target);
	}
	/**
	* Full-page convenience alias (`SmoothAgentChat.mountFullPage`): mounts the chat
	* in `mode: "fullpage"` so it fills its container/viewport with no launcher.
	*/
	function mountFullPage(config, target) {
		return mountFullPageChat(config, target);
	}
	//#endregion
	exports.SmoothAgentChatElement = SmoothAgentChatElement;
	exports.defineChatWidget = defineChatWidget;
	exports.mount = mount;
	exports.mountChatWidget = mountChatWidget;
	exports.mountFullPage = mountFullPage;
	exports.mountFullPageChat = mountFullPageChat;
	return exports;
})({});

//# sourceMappingURL=chat-widget.global.js.map