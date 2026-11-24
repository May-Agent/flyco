import { afterEach, describe, expect, it, vi } from "vitest";
import { DesktopDecoder } from "./desktopDecoder";

interface MockDecoderInstance {
  state: "unconfigured" | "configured" | "closed";
  configure: ReturnType<typeof vi.fn>;
  decode: ReturnType<typeof vi.fn>;
  close: ReturnType<typeof vi.fn>;
  init: VideoDecoderInit;
}

function installMockVideoDecoder() {
  const instances: MockDecoderInstance[] = [];

  class FakeVideoDecoder {
    state: "unconfigured" | "configured" | "closed" = "unconfigured";
    configure = vi.fn(() => {
      this.state = "configured";
    });
    decode = vi.fn();
    close = vi.fn(() => {
      this.state = "closed";
    });
    init: VideoDecoderInit;

    constructor(init: VideoDecoderInit) {
      this.init = init;
      instances.push(this as unknown as MockDecoderInstance);
    }
  }

  class FakeEncodedVideoChunk {
    type: EncodedVideoChunkType;
    timestamp: number;
    data: AllowSharedBufferSource;
    constructor(init: EncodedVideoChunkInit) {
      this.type = init.type;
      this.timestamp = init.timestamp;
      this.data = init.data;
    }
  }

  vi.stubGlobal("VideoDecoder", FakeVideoDecoder);
  vi.stubGlobal("EncodedVideoChunk", FakeEncodedVideoChunk);
  return instances;
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("DesktopDecoder", () => {
  it("recreates the decoder on async error and drops deltas until the next keyframe", () => {
    const instances = installMockVideoDecoder();
    const onError = vi.fn();
    const decoder = new DesktopDecoder({ onFrame: vi.fn(), onError });

    decoder.push(new Uint8Array([1]), true);
    expect(instances).toHaveLength(1);
    expect(instances[0]!.decode).toHaveBeenCalledTimes(1);

    // Async decoder failure closes the instance and invokes error callback.
    instances[0]!.state = "closed";
    instances[0]!.init.error(new DOMException("corrupt frame"));
    expect(onError).toHaveBeenCalledTimes(1);
    expect(instances).toHaveLength(2);
    expect(instances[1]!.state).toBe("configured");

    // Deltas pushed before recovery keyframe are dropped without calling decode.
    decoder.push(new Uint8Array([2]), false);
    expect(instances[1]!.decode).not.toHaveBeenCalled();

    // Recovery keyframe is decoded on the fresh instance.
    decoder.push(new Uint8Array([3]), true);
    expect(instances[1]!.decode).toHaveBeenCalledTimes(1);
    decoder.dispose();
  });

  it("resets a closed decoder before decoding if closed externally", () => {
    const instances = installMockVideoDecoder();
    const decoder = new DesktopDecoder({ onFrame: vi.fn(), onError: vi.fn() });

    decoder.push(new Uint8Array([1]), true);
    instances[0]!.state = "closed";

    // Delta after external close resets and drops delta until keyframe.
    decoder.push(new Uint8Array([2]), false);
    expect(instances).toHaveLength(2);
    expect(instances[1]!.decode).not.toHaveBeenCalled();

    decoder.push(new Uint8Array([3]), true);
    expect(instances[1]!.decode).toHaveBeenCalledTimes(1);
    decoder.dispose();
  });

  it("ignores push, reset, and late async callbacks after dispose()", () => {
    const instances = installMockVideoDecoder();
    const onFrame = vi.fn();
    const onError = vi.fn();
    const decoder = new DesktopDecoder({ onFrame, onError });

    decoder.push(new Uint8Array([1]), true);
    expect(instances).toHaveLength(1);

    decoder.dispose();
    decoder.push(new Uint8Array([2]), true);
    decoder.reset();
    expect(instances).toHaveLength(1);

    const frame = { close: vi.fn() } as unknown as VideoFrame;
    instances[0]!.init.output(frame);
    expect(onFrame).not.toHaveBeenCalled();
    expect(frame.close).toHaveBeenCalledTimes(1);

    instances[0]!.init.error(new DOMException("late"));
    expect(onError).not.toHaveBeenCalled();
    expect(instances).toHaveLength(1);
  });
});
