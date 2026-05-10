import { Project, SyntaxKind, JsxOpeningElement, JsxSelfClosingElement } from "ts-morph";

const project = new Project();
project.addSourceFilesAtPaths("C:/Users/sera/Desktop/shob/src/core/ui/src/**/*.{ts,tsx}");

for (const sourceFile of project.getSourceFiles()) {
  let changed = false;

  // 1. Replace imports from "solid-js" to "react"
  const imports = sourceFile.getImportDeclarations();
  for (const imp of imports) {
    if (imp.getModuleSpecifierValue() === "solid-js") {
      imp.setModuleSpecifier("react");
      changed = true;
    }
  }

  // 2. Replace solid specific keywords in the text (crude but effective for types)
  const fullText = sourceFile.getFullText();
  let newText = fullText
    .replace(/ParentProps/g, "React.PropsWithChildren")
    .replace(/Component</g, "React.FC<")
    .replace(/ParentComponent</g, "React.FC<React.PropsWithChildren<")
    .replace(/ParentComponent/g, "React.FC<React.PropsWithChildren>")
    .replace(/Accessor</g, "/* Accessor */ ")
    .replace(/createSignal/g, "useState")
    .replace(/createEffect/g, "useEffect")
    .replace(/createMemo/g, "useMemo")
    .replace(/onCleanup/g, "// onCleanup")
    .replace(/onMount/g, "useEffect");

  // 3. Replace class= with className=
  newText = newText.replace(/ class=/g, " className=");

  if (newText !== fullText) {
    sourceFile.replaceWithText(newText);
    changed = true;
  }

  if (changed) {
    sourceFile.saveSync();
  }
}

console.log("Migration script complete");
