export const getStoredValue = (key: string): string | null => {
  if (typeof window === 'undefined') return null;
  return window.localStorage.getItem(key);
};

export const setStoredValue = (key: string, value: string | null): void => {
  if (typeof window === 'undefined') return;

  if (value) {
    window.localStorage.setItem(key, value);
  } else {
    window.localStorage.removeItem(key);
  }
};
