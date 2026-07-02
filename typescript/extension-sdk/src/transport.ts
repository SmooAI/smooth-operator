/**
 * Transports carry JSON-RPC frames between two peers.
 *
 * - `stdioTransport` — the real wire: ndjson over a process's stdin/stdout,
 *   byte-for-byte the framing MCP stdio uses.
 * - `linkedPair` — two in-memory transports wired to each other, for driving an
 *   extension from an in-process test host with no subprocess.
 */
import { createInterface } from 'node:readline';
import type { Readable, Writable } from 'node:stream';
import type { JsonRpcFrame } from './jsonrpc.js';

export interface Transport {
    send(frame: JsonRpcFrame): void;
    /** Begin delivering inbound frames. Call once. */
    start(onFrame: (frame: JsonRpcFrame) => void): void;
    close(): void;
}

/** ndjson over a readable/writable pair (defaults to this process's stdio). */
export function stdioTransport(input: Readable = process.stdin, output: Writable = process.stdout): Transport {
    let rl: ReturnType<typeof createInterface> | undefined;
    return {
        send(frame) {
            output.write(`${JSON.stringify(frame)}\n`);
        },
        start(onFrame) {
            rl = createInterface({ input, terminal: false });
            rl.on('line', (line) => {
                if (!line.trim()) return;
                let frame: JsonRpcFrame;
                try {
                    frame = JSON.parse(line) as JsonRpcFrame;
                } catch {
                    // A malformed line is not a valid frame; stderr, not the wire.
                    process.stderr.write(`sep: dropping unparseable line: ${line}\n`);
                    return;
                }
                onFrame(frame);
            });
        },
        close() {
            rl?.close();
        },
    };
}

/**
 * Two transports wired to each other. A frame `send`-ed on one is delivered to
 * the other's `onFrame` on a microtask (mimicking the async wire and avoiding
 * reentrancy). Frames sent before the peer calls `start` are buffered.
 */
export function linkedPair(): [Transport, Transport] {
    const a = new InMemoryTransport();
    const b = new InMemoryTransport();
    a.peer = b;
    b.peer = a;
    return [a, b];
}

class InMemoryTransport implements Transport {
    peer!: InMemoryTransport;
    private onFrame?: (frame: JsonRpcFrame) => void;
    private buffer: JsonRpcFrame[] = [];
    private closed = false;

    send(frame: JsonRpcFrame): void {
        if (this.peer.closed) return;
        this.peer.deliver(frame);
    }

    start(onFrame: (frame: JsonRpcFrame) => void): void {
        this.onFrame = onFrame;
        const pending = this.buffer;
        this.buffer = [];
        for (const f of pending) queueMicrotask(() => this.onFrame?.(f));
    }

    close(): void {
        this.closed = true;
    }

    private deliver(frame: JsonRpcFrame): void {
        if (this.closed) return;
        if (this.onFrame) queueMicrotask(() => this.onFrame?.(frame));
        else this.buffer.push(frame);
    }
}
