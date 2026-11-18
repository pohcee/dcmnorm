// Types of the package entry point (dcmnorm.js): the native addon's API (index.d.ts), the
// generated DICOM constants (dicom-constants.d.ts) and the typed dataset API of #6.

export * from "./index";
export * from "./dicom-constants";

import type { VR, TransferSyntaxInfo } from "./dicom-constants";

/** A path, a Buffer/Uint8Array, or a readable stream / async iterable of chunks (collected first). */
export type DicomSource = string | URL | Uint8Array | AsyncIterable<Uint8Array | string>;

export type DicomErrorKind =
  | "InvalidDicom"
  | "UnsupportedTransferSyntax"
  | "Decode"
  | "Encode"
  | "Render"
  | "Dimse"
  | "Io"
  | "InvalidArgument"
  | "Internal"
  | "Other";

/** Every dataset API failure; `code` is `ERR_DCMNORM_<KIND>`. */
export declare class DcmnormError extends Error {
  readonly kind: DicomErrorKind;
  readonly code: string;
  readonly sourceTransferSyntaxUid: string | null;
  readonly targetTransferSyntaxUid: string | null;
  readonly frameIndex: number | null;
}
export declare class InvalidDicomError extends DcmnormError {}
export declare class UnsupportedTransferSyntaxError extends DcmnormError {}
export declare class DecodeError extends DcmnormError {}
export declare class EncodeError extends DcmnormError {}
export declare class RenderError extends DcmnormError {}
export declare class DimseError extends DcmnormError {}

/** A value left in its source: absolute byte offset and length (-1: undefined). */
export interface BulkDataRef {
  readonly tag: number;
  readonly vr: VR;
  readonly offset: number;
  readonly length: number;
}

export type DicomElement =
  | { readonly tag: number; readonly vr: VR; readonly type: "empty"; readonly value: null }
  | { readonly tag: number; readonly vr: VR; readonly type: "strings"; readonly value: readonly string[] }
  | { readonly tag: number; readonly vr: VR; readonly type: "ints" | "doubles" | "tags"; readonly value: readonly number[] }
  | { readonly tag: number; readonly vr: VR; readonly type: "bytes"; readonly value: Buffer }
  | { readonly tag: number; readonly vr: VR; readonly type: "bulkData"; readonly value: BulkDataRef }
  | { readonly tag: number; readonly vr: VR; readonly type: "fragments"; readonly value: { offsetTable: number[]; fragments: Buffer[] } }
  | { readonly tag: number; readonly vr: "SQ"; readonly type: "sequence"; readonly value: readonly DicomDataset[] };

export interface PersonNameGroup {
  readonly familyName: string;
  readonly givenName: string;
  readonly middleName: string;
  readonly namePrefix: string;
  readonly nameSuffix: string;
}

export declare class PersonName {
  constructor(value: string);
  readonly value: string;
  readonly alphabetic: PersonNameGroup;
  readonly ideographic: PersonNameGroup;
  readonly phonetic: PersonNameGroup;
  readonly familyName: string;
  readonly givenName: string;
}

/** A read-only data set as plain JS values. */
export declare class DicomDataset implements Iterable<DicomElement> {
  static empty(): DicomDataset;
  readonly size: number;
  readonly transferSyntaxUid: string | null;
  readonly transferSyntax: TransferSyntaxInfo | null;
  readonly fileMeta: DicomDataset;
  readonly numberOfFrames: number;
  /** FrameTime (ms), or null. */
  readonly frameTimeMillis: number | null;
  readonly frameTimeVector: number[] | null;
  /** Vertical / horizontal pixel size. */
  readonly pixelAspectRatio: number;
  [Symbol.iterator](): Iterator<DicomElement>;
  tags(): number[];
  has(tag: number): boolean;
  get(tag: number): DicomElement | undefined;
  vr(tag: number): VR | null;
  getString(tag: number, defaultValue?: string | null, index?: number): string | null;
  getStrings(tag: number): string[] | null;
  getNumber(tag: number, defaultValue?: number | null, index?: number): number | null;
  getInt(tag: number, defaultValue?: number | null, index?: number): number | null;
  getFloat(tag: number, defaultValue?: number | null, index?: number): number | null;
  getNumbers(tag: number): number[] | null;
  getInts(tag: number): number[] | null;
  getFloats(tag: number): number[] | null;
  getBytes(tag: number): Buffer | null;
  getBulkDataRef(tag: number): BulkDataRef | null;
  getSequence(tag: number): DicomDataset[] | null;
  getNestedDataset(tag: number, index?: number): DicomDataset | null;
  getPersonName(tag: number, index?: number): PersonName | null;
  toBuilder(): DicomDatasetBuilder;
}

export declare class DicomDatasetBuilder {
  set(tag: number, vr: VR, ...values: Array<string | number>): this;
  set(tag: number, values: Array<string | number>): this;
  setInt(tag: number, vr: VR, ...values: number[]): this;
  setDouble(tag: number, vr: VR, ...values: number[]): this;
  setTags(tag: number, ...tags: number[]): this;
  setBytes(tag: number, vr: VR, bytes: Uint8Array): this;
  setNull(tag: number, vr: VR): this;
  setSequence(tag: number, items: DicomDataset[]): this;
  addSequenceItem(tag: number, item: DicomDataset): this;
  editSequenceItem(tag: number, index: number, edit: (item: DicomDatasetBuilder) => void): this;
  removeSequenceItem(tag: number, index: number): this;
  remove(tag: number): this;
  removePrivateTags(): this;
  build(): DicomDataset;
}

export interface ReadOptions {
  /** Stop at the PixelData header (PixelData is then a BulkDataRef). */
  untilPixelData?: boolean;
  /** Read bulk values instead of leaving them in the source as references. */
  loadBulkData?: boolean;
}

export interface DicomJsonOptions {
  includeTags?: number[];
  excludeTags?: number[];
  bulkData?: "uri" | "inline";
  bulkDataUriBase?: string;
  bulkDataUriBuilder?: (ref: BulkDataRef) => string;
  includeFileMeta?: boolean;
  keywordAndVM?: boolean;
}

export interface WriteDatasetOptions {
  /** Transcode to this transfer syntax UID. */
  transferSyntax?: string;
  /** Where the data set's bulk data references are read from, instead of its own source. */
  bulkDataSource?: string | Uint8Array;
}

export interface RenderParams {
  windowCenter?: number;
  windowWidth?: number;
  voiLut?: boolean;
  modalityLut?: boolean;
  iccProfile?: boolean;
  outputWidth?: number;
  outputHeight?: number;
  showOverlays?: boolean;
  overlayIndex?: number;
  overlayColor?: string;
}

export interface RenderedPixels {
  width: number;
  height: number;
  /** 1 (gray) or 3 (RGB interleaved). */
  channels: number;
  /** width * height * channels bytes, rows top to bottom. */
  pixels: Buffer;
  overlays: Array<{ index: number; group: number; rows: number; columns: number; overlayType?: string; label?: string }>;
  selectedOverlayIndex?: number;
}

export declare function read(source: DicomSource, options?: ReadOptions): Promise<DicomDataset>;
export declare function toDicomJson(source: DicomSource | DicomDataset, options?: DicomJsonOptions): Promise<string>;
export declare function write(dataset: DicomDataset, outputPath: string, options?: WriteDatasetOptions): Promise<void>;
export declare function write(dataset: DicomDataset, outputPath: null, options?: WriteDatasetOptions): Promise<Buffer>;
export declare function toBuffer(dataset: DicomDataset, options?: WriteDatasetOptions): Promise<Buffer>;
export declare function renderPixels(source: DicomSource, frameIndex?: number, params?: RenderParams): Promise<RenderedPixels>;
export declare function transcodeTo(source: DicomSource, transferSyntaxUid: string, outputPath: string): Promise<void>;
export declare function transcodeTo(source: DicomSource, transferSyntaxUid: string, outputPath?: null): Promise<Buffer>;
export declare function sopClassName(uid: string): string | null;
