// Diagnostic only. Field slots follow Microsoft's PSEC 1.1 FlatBuffers schema:
// external/windows-sdk/ProcessSecurityEnvironment.fbs, SHA-256
// 736cacef37a6c97a0cb97c7132da462a3b51811fca8f1e4076cac51085dfa516.
import { Builder } from 'flatbuffers';

export function encodeNativePolicy(request) {
  const builder = new Builder(4096);
  const vector = values => {
    const strings = values.map(value => builder.createString(value));
    builder.startVector(4, strings.length, 4);
    for (const value of strings.toReversed()) builder.addOffset(value);
    return builder.endVector();
  };
  const writable = vector(request.filesystem.readwritePaths);
  const readable = vector(request.filesystem.readonlyPaths);
  const denied = vector(request.filesystem.deniedPaths);
  const enumerable = vector(request.containment.config.filesystem.enumeratePaths);
  const capabilities = builder.createString('registryRead');
  // EndpointPolicy and IngressPolicy both default to deny. Empty tables
  // explicitly select those defaults; no proxy or allowed peer is supplied.
  builder.startObject(3);
  const egress = builder.endObject();
  builder.startObject(2);
  const ingress = builder.endObject();
  builder.startObject(4);
  builder.addFieldOffset(1, egress, 0);
  builder.addFieldOffset(3, ingress, 0);
  const network = builder.endObject();
  builder.startObject(9);
  builder.addFieldOffset(1, capabilities, 0);
  builder.addFieldOffset(4, writable, 0);
  builder.addFieldOffset(5, readable, 0);
  builder.addFieldOffset(6, denied, 0);
  builder.addFieldOffset(7, network, 0);
  builder.addFieldOffset(8, enumerable, 0);
  builder.prep(2, 4);
  builder.writeInt16(1); // minor
  builder.writeInt16(1); // major
  builder.addFieldStruct(0, builder.offset(), 0);
  const policy = builder.endObject();
  builder.requiredField(policy, 4);
  builder.finish(policy, 'PSEC');
  return Buffer.from(builder.asUint8Array());
}
