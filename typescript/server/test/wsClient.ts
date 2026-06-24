/**
 * A tiny promise-based WebSocket test client over the `ws` package — the TS parity
 * of the C# integration test's `ConnectAsync` / `SendAsync` / `ReceiveAsync`
 * helpers. Drives a REAL socket against the booted server.
 *
 * Incoming frames are BUFFERED as they arrive (the server can emit several in one
 * tick — ack + tokens + terminal); `receive()` drains the buffer, so no frame is
 * lost between calls.
 */
import { WebSocket } from 'ws';

export class TestClient {
    private readonly buffer: Record<string, unknown>[] = [];
    private waiter: (() => void) | undefined;
    private closed = false;

    private constructor(private readonly socket: WebSocket) {
        socket.on('message', (data: Buffer | ArrayBuffer | Buffer[]) => {
            const text = Array.isArray(data)
                ? Buffer.concat(data).toString('utf8')
                : data instanceof ArrayBuffer
                  ? Buffer.from(data).toString('utf8')
                  : (data as Buffer).toString('utf8');
            this.buffer.push(JSON.parse(text) as Record<string, unknown>);
            this.wake();
        });
        socket.on('close', () => {
            this.closed = true;
            this.wake();
        });
    }

    static async connect(url: string): Promise<TestClient> {
        const socket = new WebSocket(url);
        await new Promise<void>((resolve, reject) => {
            socket.once('open', resolve);
            socket.once('error', reject);
        });
        return new TestClient(socket);
    }

    private wake(): void {
        const w = this.waiter;
        this.waiter = undefined;
        w?.();
    }

    send(json: string): void {
        this.socket.send(json);
    }

    sendAction(action: Record<string, unknown>): void {
        this.socket.send(JSON.stringify(action));
    }

    /** Receive the next buffered frame (waiting if necessary). Rejects on timeout/close. */
    async receive(timeoutMs = 5000): Promise<Record<string, unknown>> {
        const deadline = Date.now() + timeoutMs;
        for (;;) {
            const frame = this.buffer.shift();
            if (frame !== undefined) return frame;
            if (this.closed) throw new Error('socket closed before a frame arrived');
            const remaining = deadline - Date.now();
            if (remaining <= 0) throw new Error('timed out waiting for a frame');
            await new Promise<void>((resolve) => {
                const timer = setTimeout(resolve, remaining);
                this.waiter = () => {
                    clearTimeout(timer);
                    resolve();
                };
            });
        }
    }

    /** Receive frames until one of `type` arrives; returns it (and all seen frames). */
    async receiveUntil(type: string, timeoutMs = 8000): Promise<{ terminal: Record<string, unknown>; seen: Record<string, unknown>[] }> {
        const seen: Record<string, unknown>[] = [];
        const deadline = Date.now() + timeoutMs;
        for (;;) {
            const remaining = deadline - Date.now();
            if (remaining <= 0) throw new Error(`timed out waiting for a '${type}' frame`);
            const frame = await this.receive(remaining);
            seen.push(frame);
            if (frame.type === type) return { terminal: frame, seen };
        }
    }

    async close(): Promise<void> {
        if (this.socket.readyState === WebSocket.CLOSED) return;
        await new Promise<void>((resolve) => {
            this.socket.once('close', () => resolve());
            this.socket.close(1000, 'done');
        });
    }
}
