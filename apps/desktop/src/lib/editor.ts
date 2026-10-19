import { api } from "./api";
import { toast } from "./toast";

/** Opens the project, or a project-relative file, in the installed editor. */
export async function openInEditor(projectId: string, path?: string) {
  try {
    const editor = await api.openInEditor(projectId, path);
    toast(`Opened ${path ?? "the project"} in ${editor}`);
  } catch (error) {
    toast(String(error), "error");
  }
}
