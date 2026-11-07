import { host, mount, signal, Text, View } from "nsh";
import { Input } from "nsh/base";

function Panel() {
  const draft = signal("");
  const status = signal("Checking editor API…");
  host.call("editor.handshake").then((api: unknown) => {
    if (api && typeof api === "object" && "editorApiVersion" in api && api.editorApiVersion === 1) {
      status.set("Editor API 1");
    } else {
      status.set("Unsupported editor API");
    }
  }).catch(() => status.set("Handshake failed"));
  return <View class="flex-1 gap-4 bg-white p-5 dark:bg-zinc-900">
    <Text class="text-xl text-zinc-900 dark:text-zinc-100">NSH isolated panel evaluation</Text>
    <Text class="text-zinc-600 dark:text-zinc-300">{status()}</Text>
    <Input value={draft()} onChange={({ value }) => draft.set(value)} placeholder="IME / Tab / focus probe" class="min-h-12 border border-zinc-500 p-3 text-zinc-900 dark:text-zinc-100" />
    <Text class="text-zinc-600 dark:text-zinc-300">{draft()}</Text>
    <View role="button" onPress={() => { throw new Error("Intentional panel exception"); }} class="rounded bg-zinc-200 p-3">
      <Text>Throw script exception</Text>
    </View>
    <View role="button" onPress={() => { while (true) {} }} class="rounded bg-zinc-200 p-3">
      <Text>Run bounded loop</Text>
    </View>
  </View>;
}
mount(Panel);
