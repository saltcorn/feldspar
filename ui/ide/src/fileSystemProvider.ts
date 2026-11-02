/**
 * The file store, as a VS Code filesystem (design §12.1).
 *
 * A translation layer and deliberately nothing more: URIs to store paths, store
 * entries to `FileType`, [`StoreFileError`] to the codes VS Code acts on. The
 * operations themselves are `storeFiles.ts`, which is where the API lives.
 *
 * The provider is registered as an **overlay** on the `file` scheme, so the
 * workspace folder `/<store>` is served by the store's API while everything else
 * VS Code keeps in memory (its own settings, extension files) is untouched.
 */

import {
  FileChangeType,
  FileSystemProviderCapabilities,
  FileSystemProviderErrorCode,
  FileType,
  registerFileSystemOverlay,
  type IFileChange,
  type IFileDeleteOptions,
  type IFileOverwriteOptions,
  type IFileSystemProviderWithFileReadWriteCapability,
  type IFileWriteOptions,
  type IStat,
} from "@codingame/monaco-vscode-files-service-override";
// `createFileSystemProviderError` is the only way to build an error carrying a
// code (the class's constructor is private), and it is not among the override
// package's re-exports — hence the deep import into the API package.
import { createFileSystemProviderError } from "@codingame/monaco-vscode-api/vscode/vs/platform/files/common/files";
import { Emitter, Event } from "@codingame/monaco-vscode-api/vscode/vs/base/common/event";
import {
  Disposable,
  type IDisposable,
} from "@codingame/monaco-vscode-api/vscode/vs/base/common/lifecycle";
import type { URI } from "@codingame/monaco-vscode-api/vscode/vs/base/common/uri";

import { StoreFileError, StoreFiles, toStorePath, type EntryKind } from "./storeFiles";

/**
 * The store has no timestamps, so every path reports the same ones.
 *
 * This is honest rather than lossy: `mtime` exists for VS Code to notice that a
 * file changed underneath an open editor, and a constant is the correct answer
 * from a store that cannot tell us when it did. A *made-up* clock would be worse
 * — it would manufacture conflicts on every save.
 */
const NO_TIME = 0;

export class StoreFileSystemProvider implements IFileSystemProviderWithFileReadWriteCapability {
  /**
   * `FileReadWrite`, and no more. Not `FileOpenReadWriteClose` (there is no file
   * handle behind an HTTP endpoint), not `FileAtomicWrite` (the store writes a
   * whole file, but says nothing about how), and not `Readonly` — this is the
   * capability set the API actually backs.
   */
  readonly capabilities =
    FileSystemProviderCapabilities.FileReadWrite | FileSystemProviderCapabilities.PathCaseSensitive;

  private readonly _onDidChangeFile = new Emitter<readonly IFileChange[]>();
  readonly onDidChangeFile: Event<readonly IFileChange[]> = this._onDidChangeFile.event;
  readonly onDidChangeCapabilities = Event.None;

  constructor(private readonly files: StoreFiles) {}

  /**
   * There is no watcher, and there cannot be one: the file API is request/
   * response, with nothing that pushes.
   *
   * So changes made *here* are announced (each operation below fires its own
   * event) and changes made anywhere else — a git pull on the store, another
   * admin, the build writing `dist/` — are seen when the admin refreshes the
   * explorer. Returning a no-op disposable rather than throwing is what keeps
   * that a limitation instead of a failure.
   */
  watch(): IDisposable {
    return Disposable.None;
  }

  async stat(resource: URI): Promise<IStat> {
    const path = this.path(resource);
    const entry = await this.run(() => this.files.stat(path));
    if (entry === null) {
      throw createFileSystemProviderError(
        `${path === "" ? this.files.store : path} does not exist`,
        FileSystemProviderErrorCode.FileNotFound,
      );
    }
    return {
      type: fileType(entry.kind),
      mtime: NO_TIME,
      ctime: NO_TIME,
      size: entry.size,
    };
  }

  async readdir(resource: URI): Promise<[string, FileType][]> {
    const entries = await this.run(() => this.files.list(this.path(resource)));
    return entries.map((entry) => [entry.name, fileType(entry.kind)]);
  }

  async readFile(resource: URI): Promise<Uint8Array> {
    return this.run(() => this.files.read(this.path(resource)));
  }

  async writeFile(resource: URI, content: Uint8Array, opts: IFileWriteOptions): Promise<void> {
    const path = this.path(resource);
    // The store's write always creates and always overwrites, so the options are
    // checked only when they *forbid* something — one extra request on the paths
    // that ask for a guarantee, none on an ordinary save.
    if (!opts.create || !opts.overwrite) {
      const existing = await this.run(() => this.files.stat(path));
      if (existing === null && !opts.create) {
        throw createFileSystemProviderError(
          `${path} does not exist`,
          FileSystemProviderErrorCode.FileNotFound,
        );
      }
      if (existing !== null && !opts.overwrite) {
        throw createFileSystemProviderError(
          `${path} already exists`,
          FileSystemProviderErrorCode.FileExists,
        );
      }
    }
    await this.run(() => this.files.write(path, content));
    this.announce(resource, FileChangeType.UPDATED);
  }

  async mkdir(resource: URI): Promise<void> {
    await this.run(() => this.files.makeDirectory(this.path(resource)));
    this.announce(resource, FileChangeType.ADDED);
  }

  async delete(resource: URI, _opts: IFileDeleteOptions): Promise<void> {
    // `recursive` is not honoured because the store has no non-recursive delete:
    // deleting a directory takes its contents with it. VS Code only asks
    // non-recursively for a file, where the distinction does not arise.
    await this.run(() => this.files.remove(this.path(resource)));
    this.announce(resource, FileChangeType.DELETED);
  }

  async rename(from: URI, to: URI, opts: IFileOverwriteOptions): Promise<void> {
    await this.run(() => this.files.move(this.path(from), this.path(to), opts.overwrite));
    this._onDidChangeFile.fire([
      { type: FileChangeType.DELETED, resource: from },
      { type: FileChangeType.ADDED, resource: to },
    ]);
  }

  /**
   * Say that these paths changed outside the IDE, so what is open on them is
   * re-read.
   *
   * The counterpart to [`watch`](StoreFileSystemProvider::watch) returning
   * nothing: no watcher can *notice* an outside change, but the IDE sometimes
   * **causes** one — a pull, a branch switch, a build — and in those moments it
   * knows exactly as much as a watcher would have told it. Announcing then is
   * what makes an open editor show the branch that was just checked out instead
   * of the one that was.
   */
  announceChanged(resources: readonly URI[]): void {
    if (resources.length === 0) return;
    this._onDidChangeFile.fire(
      resources.map((resource) => ({ type: FileChangeType.UPDATED, resource })),
    );
  }

  /**
   * Say that these paths may have appeared, and that these are gone, outside
   * the IDE.
   *
   * What the explorer redraws for: it ignores an update, redraws a directory
   * when a path in it is added that it does not show yet, and drops a path
   * that is deleted. "May have appeared" because an added path the explorer
   * already shows costs nothing, so the caller need not know which are new.
   */
  announceAddedAndRemoved(added: readonly URI[], removed: readonly URI[]): void {
    if (added.length === 0 && removed.length === 0) return;
    this._onDidChangeFile.fire([
      ...added.map((resource) => ({ type: FileChangeType.ADDED, resource })),
      ...removed.map((resource) => ({ type: FileChangeType.DELETED, resource })),
    ]);
  }

  /** The store-relative path a URI names. */
  private path(resource: URI): string {
    return toStorePath(this.files.store, resource.path);
  }

  private announce(resource: URI, type: FileChangeType): void {
    this._onDidChangeFile.fire([{ type, resource }]);
  }

  /** Run a store operation, translating its failure into VS Code's vocabulary. */
  private async run<T>(operation: () => Promise<T>): Promise<T> {
    try {
      return await operation();
    } catch (err) {
      throw err instanceof StoreFileError ? toProviderError(err) : err;
    }
  }
}

/** A store entry's kind, as VS Code's [`FileType`]. */
export function fileType(kind: EntryKind): FileType {
  return kind === "directory" ? FileType.Directory : FileType.File;
}

/** A store failure, as the code VS Code decides what to do from. */
export function toProviderError(err: StoreFileError): Error {
  switch (err.kind) {
    case "notFound":
      return createFileSystemProviderError(err.message, FileSystemProviderErrorCode.FileNotFound);
    case "notADirectory":
      return createFileSystemProviderError(
        err.message,
        FileSystemProviderErrorCode.FileNotADirectory,
      );
    case "exists":
      return createFileSystemProviderError(err.message, FileSystemProviderErrorCode.FileExists);
    case "noPermission":
      return createFileSystemProviderError(err.message, FileSystemProviderErrorCode.NoPermissions);
    default:
      return createFileSystemProviderError(err.message, FileSystemProviderErrorCode.Unknown);
  }
}

/**
 * Serve the workspace folder from `files`, for as long as the page lives,
 * returning the provider — source control needs it to say that a pull or a
 * checkout changed what is open.
 */
export function registerStoreFileSystem(files: StoreFiles): StoreFileSystemProvider {
  const provider = new StoreFileSystemProvider(files);
  registerFileSystemOverlay(1, provider);
  return provider;
}
