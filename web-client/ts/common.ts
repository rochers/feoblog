// Common classes/functions for FeoBlog UI.

import bs58 from "bs58"
import * as commonmark from "commonmark"
import { DateTime } from "luxon";
import type { Writable } from "svelte/store";

const USER_ID_BYTES = 32;
const PASSWORD_BYTES = USER_ID_BYTES + 4 // 4 bytes b58 checksum.

export const MAX_ITEM_SIZE = 32 * 1024 // 32k

// TODO: Deprecated. Use client.UserID.fromString() instead.
// Parse a base58 userID to a Uint8Array or throws a string error message.
export function parseUserID(userID: string): Uint8Array {
    if (userID.length == 0) {
        throw "UserID must not be empty."
    }

    let buf: Uint8Array;
    try {
        buf = bs58.decode(userID)
    } catch (error) {
        throw "Not valid base58"
    }

    if (buf.length < USER_ID_BYTES) {
        throw "UserID too short"
    }

    if (buf.length == PASSWORD_BYTES) {
        throw "UserID too long. (This may be a paswword!?)"
    }

    if (buf.length > USER_ID_BYTES) {
        throw "UserID too long."
    }

    return buf
}

export function parseUserIDError(userID: string): string {
    try {
        parseUserID(userID)
    } catch (errorMessage) {
        return errorMessage
    }
    return ""
}

const cmReader = new commonmark.Parser()
const cmWriter = new commonmark.HtmlRenderer({ safe: true})

export function markdownToHtml(markdown: string): string {
    let parsed = cmReader.parse(markdown)
    return cmWriter.render(parsed)
}

// Applies `asyncFilter` to up to `count` items before it begins yielding them.
// Useful for prefetching things in parallel with promises.
export async function* prefetch<T, Out>(items: AsyncIterable<T>, count: Number, asyncFilter: (t: T) => Promise<Out>): AsyncGenerator<Out> {
    let outs: Promise<Out>[] = []

    for await (let item of items) {
        outs.push(asyncFilter(item))
        while (outs.length > count) {
            yield assertExists(outs.shift())
        }
    }

    while (outs.length > 0) {
        yield assertExists(outs.shift())
    }
}

// TypeScript doesn't know that we've done our own bounds checking on things like Array.shift()
// Assert that we have:
function assertExists<T>(value: T|undefined): T {
    return value as T
}

// A small subset of the Console interface
export interface Logger {
    debug(...data: any[]): void;
    error(...data: any[]): void;
    info(...data: any[]): void;
    log(...data: any[]): void;
    warn(...data: any[]): void;
}

export class ConsoleLogger implements Logger {

    error(...data: any[]): void {
        console.error(...data)

    }
    warn(...data: any[]): void {
        console.warn(...data)
    }

    // without this, only warn & error show:
    private debugEnabled = false

    debug(...data: any[]): void {
        if (this.debugEnabled) console.debug(...data)
    }
    info(...data: any[]): void {
        if (this.debugEnabled) console.info(...data)
    }
    log(...data: any[]): void {
        // I tend to treat this like a debug statement, so:
        if (this.debugEnabled) console.log(...data)
    }

    withDebug(): ConsoleLogger {
        this.debugEnabled = true
        return this
    }
}


// Tracks the progress of some long-running async task.
export class TaskTracker 
{
    // A store that will get updated every time this object changes
    store: Writable<TaskTracker>|null = null
 
    _isRunning = false
    get isRunning() { return this._isRunning }

    _logs: LogEntry[] = []
    get logs(): ReadonlyArray<LogEntry> {
        return this._logs
    }

    async run(asyncTask: () => Promise<void>): Promise<void> {
        this.clear()
        this._isRunning = true
        this.log("Begin") // calls notify()
        try {
            await asyncTask()
        } catch (e) {
            this.error(`Task threw an exception: ${e}`)
        }
        this._isRunning = false
        this.log("Done") // calls notify()
    }

    private notify() {
        if (this.store) this.store.set(this)
    }

    clear() {
        this._logs = []
        this.notify()
    }

    private writeLog(log: LogEntry) {
        this._logs.push(log)
        this.notify()
    }

    error(message: string) {
        this.writeLog({
            message,
            isError: true,
            timestamp: DateTime.local().valueOf()
        })
    }

    log(message: string) {
        this.writeLog({
            message,
            timestamp: DateTime.local().valueOf()
        })
    }

    warn(message: string) {
        this.writeLog({
            message,
            isWarning: true,
            timestamp: DateTime.local().valueOf()
        })
    }
}


type LogEntry = {
    timestamp: number
    message: string
    isError?: boolean
    isWarning?: boolean
}


const serverURLPattern = /^(https?:\/\/[^/ ]+)$/
// Returns a non-empty error string if `url` is not a valid server URL.
export function validateServerURL(url: string): string {
    if (url === "") {
        return "" // Don't show error in the empty case.
    }

    let match = serverURLPattern.exec(url)
    if (match === null) {
        return "Invalid server URL format"
    }

    return ""
}