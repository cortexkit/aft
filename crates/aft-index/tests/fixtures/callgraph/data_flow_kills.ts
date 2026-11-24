function sink(input: string): void {
    console.log(input);
}

export function overwritten(raw: string): void {
    let alias = raw;
    alias = "replacement";
    sink(alias);
}

export function derived(raw: { field: string }): void {
    let alias = "initial";
    alias = raw.field;
    sink(alias);
}

export function usedBeforeOverwrite(raw: string): void {
    let alias = raw;
    sink(alias);
    alias = "replacement";
}

export function conditionallyOverwritten(raw: string, condition: boolean): void {
    let alias = raw;
    if (condition) {
        alias = "replacement";
    }
    sink(alias);
}

export function augmented(raw: string): void {
    let alias = raw;
    alias += "suffix";
    sink(alias);
}
