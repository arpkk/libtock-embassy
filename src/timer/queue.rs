//! Cola de plazos de `timer`, sin syscalls: `timer` le entrega la hora y ejecuta
//! lo que la cola decide.
//!
//! Es una lista doblemente enlazada e intrusiva, ordenada por vencimiento. Cada
//! nodo vive dentro de su `Timer`, así que no hay capacidad fija ni memoria
//! reservada para timers que no existen. Cada nodo guarda su plazo como delta
//! respecto del anterior, y la cabeza respecto de `anchor`, la última hora
//! conocida. Así ningún par de plazos se compara con aritmética modular: solo se
//! resta lo transcurrido desde `anchor`.
//!
//! Un plazo más largo que [`SEGMENT`] se recorre en tramos: al vencer cada uno,
//! el nodo vuelve a la lista en vez de despertar a su tarea.
#![allow(unsafe_code)] // Punteros entre nodos que viven dentro de los `Timer`.

use core::cell::Cell;
use core::marker::PhantomPinned;
use core::ptr::NonNull;
use core::task::Waker;

/// Plazo más largo que se arma en una sola alarma: media vuelta del contador.
/// Deja otra media vuelta para reconocer un plazo vencido aunque se atienda
/// tarde. Los plazos más largos se recorren en tramos de este largo.
pub(super) const SEGMENT: u32 = 1 << 31;

/// En qué lista está un nodo.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Link {
    /// En ninguna: todavía no se ha hecho `poll`.
    Idle,
    /// En `pending`: espera que `flush` le asigne la hora de referencia.
    Pending,
    /// En la lista de plazos.
    Linked,
    /// Venció y ya salió de las listas.
    Fired,
}

/// Nodo de la cola. Es `!Unpin` para que el compilador no suponga que un
/// `&mut Timer` es el único acceso a esta memoria, y todos sus campos son `Cell`
/// porque la cola los modifica a través de punteros compartidos.
pub(super) struct Node {
    prev: Cell<Option<NonNull<Node>>>, //anterior
    next: Cell<Option<NonNull<Node>>>, //siguiente
    /// En `Linked`, ticks desde el nodo anterior o desde `anchor` si es la cabeza.
    /// En `Pending`, el `dt` pedido.
    delta: Cell<u32>, //cuánto falta desde el nodo anterior o desde la ultima hora conocida si es la cabeza en ticks
    /// Tramos completos de [`SEGMENT`] que faltan después del plazo actual.
    segments: Cell<u32>,
    waker: Cell<Option<Waker>>, //waker de la tarea
    link: Cell<Link>, //en qué estado esta: Idle (sin poll aun), Pending (esperando flush), Linked (en la lista) o Fired (venció)
    _pin: PhantomPinned,
}

impl Node {
    pub(super) const fn new() -> Self {
        Self {
            prev: Cell::new(None),
            next: Cell::new(None),
            delta: Cell::new(0),
            segments: Cell::new(0),
            waker: Cell::new(None),
            link: Cell::new(Link::Idle),
            _pin: PhantomPinned,
        }
    }

    pub(super) fn link(&self) -> Link {
        self.link.get()
    }

    /// Fija cuántos tramos de [`SEGMENT`] siguen al primer plazo.
    pub(super) fn set_segments(&self, segments: u32) {
        self.segments.set(segments);
    }

    /// Si quedan tramos, descuenta los que ya pasaron, dado que el plazo actual
    /// lleva `late` ticks vencido, y devuelve cuánto falta para el siguiente.
    /// `None` si el plazo final ya venció.
    fn next_segment(&self, mut late: u32) -> Option<u32> {
        let mut left = self.segments.get();
        while left > 0 {
            left -= 1;
            if late < SEGMENT {
                self.segments.set(left);
                return Some(SEGMENT - late);
            }
            late -= SEGMENT;
        }
        self.segments.set(0);
        None
    }

    /// Marca el nodo como vencido sin haber pasado por las listas.
    pub(super) fn set_fired(&self) {
        self.waker.set(None);
        self.link.set(Link::Fired);
    }

    /// Guarda el waker, sin clonar si ya es el mismo.
    pub(super) fn register(&self, waker: &Waker) {
        let waker = match self.waker.take() {
            Some(old) if old.will_wake(waker) => old,
            _ => waker.clone(),
        };
        self.waker.set(Some(waker));
    }

    fn fire(&self) {
        self.prev.set(None);
        self.next.set(None);
        self.link.set(Link::Fired);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

/// Lo que se pidió en el último `command 6` y lo que devolvió el kernel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Armed {
    pub(super) reference: u32,
    pub(super) dt: u32,
    /// Vencimiento redondeado por la cápsula. La upcall lo trae en `arg1`.
    pub(super) expiration: u32,
}

/// Qué tiene que hacer `timer` con la alarma del kernel al final de una pasada.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Action {
    /// Nada cambió.
    Nothing,
    /// La cabeza cambió pero la alarma armada ya sirve.
    Skip,
    /// `command 6` con estos argumentos.
    Arm { reference: u32, dt: u32 },
    /// No quedan plazos y la alarma armada no venció: `command 3`.
    Stop,
}

pub(super) struct Queue {
    head: Option<NonNull<Node>>, //lista de plazos
    pending: Option<NonNull<Node>>, //lista pendientes
    anchor: u32, //última hora conocida en ticks del contador
    armed: Option<Armed>, //última alarma armada, si no venció
    /// La cabeza cambió desde la última decisión.
    dirty: bool, //true si la cabeza cambió desde la última decisión
}

// SAFETY: el proceso es de un solo hilo y la cola solo se usa dentro de
// `critical_section::with`, así que los punteros nunca cruzan hilos.
unsafe impl Send for Queue {}

/// Acceso a un nodo enlazado.
///
/// SAFETY: un nodo está en una lista solo entre su primer `poll`, cuando ya está
/// fijado en memoria (`Pin`), y su `Drop` o su vencimiento, que lo sacan. La
/// garantía de `Drop` de `Pin` asegura que la memoria no se reutiliza antes.
fn node<'a>(ptr: NonNull<Node>) -> &'a Node {
    unsafe { ptr.as_ref() }
}

impl Queue {
    pub(super) const fn new() -> Self {
        Self {
            head: None,
            pending: None,
            anchor: 0,
            armed: None,
            dirty: false,
        }
    }

    pub(super) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// `true` si `t` no es anterior a `anchor`. Una upcall puede traer una hora
    /// tomada antes de la última lectura, y retroceder `anchor` adelantaría todos
    /// los plazos.
    pub(super) fn is_current(&self, t: u32) -> bool {
        t.wrapping_sub(self.anchor) as i32 >= 0
    }

    /// Lleva `anchor` a `now` y despierta los plazos vencidos.
    pub(super) fn advance(&mut self, now: u32) { //equivalente a next_expiration(now), pone la cola al dia
        let mut elapsed = now.wrapping_sub(self.anchor);
        self.anchor = now;
        // Nodos a los que les quedan tramos, encadenados por `next`. Vuelven a la
        // lista cuando esta queda al día, contados desde `now`.
        let mut relink = None;
        while let Some(ptr) = self.head {
            let head = node(ptr);
            let delta = head.delta.get();
            if delta > elapsed {
                head.delta.set(delta - elapsed);
                break;
            }
            elapsed -= delta;
            self.head = head.next.get();
            if let Some(next) = self.head {
                node(next).prev.set(None);
            }
            self.dirty = true;
            match head.next_segment(elapsed) {
                Some(offset) => {
                    head.delta.set(offset);
                    head.next.set(relink);
                    relink = Some(ptr);
                }
                None => head.fire(),
            }
        }
        while let Some(ptr) = relink {
            let entry = node(ptr);
            relink = entry.next.get();
            self.link_sorted(entry, entry.delta.get());
        }
    }

    /// Agrega un plazo de `dt` ticks que se empezará a contar en el próximo `flush`.
    pub(super) fn push_pending(&mut self, entry: &Node, dt: u32) {
        let ptr = NonNull::from(entry);
        entry.delta.set(dt);
        entry.prev.set(None);
        entry.next.set(self.pending);
        entry.link.set(Link::Pending);
        if let Some(first) = self.pending {
            node(first).prev.set(Some(ptr));
        }
        self.pending = Some(ptr);
    }

    /// Agrega un plazo de `dt` ticks contados desde `reference`, más los tramos
    /// del nodo. `reference` no puede ser posterior a la última hora conocida.
    /// Devuelve `false`, sin agregarlo, si el plazo final ya venció.
    pub(super) fn insert(&mut self, entry: &Node, reference: u32, dt: u32) -> bool {
        let gone = self.anchor.wrapping_sub(reference);
        let offset = match gone.checked_sub(dt) {
            None => dt - gone,
            Some(late) => match entry.next_segment(late) {
                Some(offset) => offset,
                None => return false,
            },
        };
        self.link_sorted(entry, offset);
        true
    }

    /// Lleva `anchor` a `now` y pasa los plazos pendientes a la lista, contados
    /// desde ahí.
    pub(super) fn flush_pending(&mut self, now: u32) {
        self.advance(now);
        while let Some(ptr) = self.pending {
            let entry = node(ptr);
            self.pending = entry.next.get();
            if let Some(next) = self.pending {
                node(next).prev.set(None);
            }
            self.link_sorted(entry, entry.delta.get());
        }
    }

    /// Saca un nodo de su lista. Si estaba en la de plazos, su delta pasa al siguiente.
    pub(super) fn remove(&mut self, entry: &Node) {
        let pending = match entry.link.get() {
            Link::Pending => true,
            Link::Linked => false,
            Link::Idle | Link::Fired => return,
        };
        let prev = entry.prev.get();
        let next = entry.next.get();
        if let Some(next) = next {
            let after = node(next);
            if !pending {
                after.delta.set(after.delta.get() + entry.delta.get());
            }
            after.prev.set(prev);
        }
        match (prev, pending) {
            (Some(prev), _) => node(prev).next.set(next),
            (None, true) => self.pending = next,
            (None, false) => {
                self.head = next;
                self.dirty = true;
            }
        }
        entry.prev.set(None);
        entry.next.set(None);
        entry.link.set(Link::Idle);
    }

    /// Inserta en orden, `offset` ticks después de `anchor`. Si hay plazos iguales,
    /// el nuevo queda después.
    fn link_sorted(&mut self, entry: &Node, mut offset: u32) {
        let ptr = NonNull::from(entry);
        let mut prev = None;
        let mut cur = self.head;
        while let Some(c) = cur {
            let here = node(c);
            let delta = here.delta.get();
            if offset < delta {
                here.delta.set(delta - offset);
                break;
            }
            offset -= delta;
            prev = cur;
            cur = here.next.get();
        }
        entry.delta.set(offset);
        entry.prev.set(prev);
        entry.next.set(cur);
        entry.link.set(Link::Linked);
        if let Some(c) = cur {
            node(c).prev.set(Some(ptr));
        }
        match prev {
            Some(p) => node(p).next.set(Some(ptr)),
            None => {
                self.head = Some(ptr);
                self.dirty = true;
            }
        }
    }

    /// Decide qué hacer con la alarma del kernel. Usa `anchor` como hora, así que
    /// no necesita leer el contador.
    ///
    /// Si la alarma armada ya venció, su upcall está en cola y al atenderla se
    /// vuelve a decidir; re-armar ahora encolaría otra upcall. Si lleva más de
    /// `lost_after` ticks vencida sin que llegue, se da por perdida.
    pub(super) fn decide(&mut self, lost_after: u32) -> Action {
        let overdue = self.armed.and_then(|a| {
            let gone = self.anchor.wrapping_sub(a.reference);
            (gone >= a.dt).then(|| gone - a.dt)
        });
        if !self.dirty && overdue.is_none() {
            return Action::Nothing;
        }
        let changed = core::mem::replace(&mut self.dirty, false);
        match overdue {
            Some(late) if late < lost_after => {
                return if changed { Action::Skip } else { Action::Nothing };
            }
            Some(_) => self.armed = None,
            None => {}
        }
        match (self.head, self.armed) {
            (Some(head), armed) => {
                let dt = node(head).delta.get();
                let deadline = self.anchor.wrapping_add(dt);
                match armed {
                    Some(a) if a.reference.wrapping_add(a.dt) == deadline => Action::Skip,
                    _ => Action::Arm {
                        reference: self.anchor,
                        dt,
                    },
                }
            }
            (None, Some(_)) => Action::Stop,
            (None, None) => Action::Nothing,
        }
    }

    pub(super) fn set_armed(&mut self, armed: Option<Armed>) {
        self.armed = armed;
    }

    /// Atiende una upcall de alarma. Si no es la del último `command 6`, viene de
    /// una alarma ya reemplazada y solo aporta la hora.
    pub(super) fn on_upcall(&mut self, now: u32, expiration: u32) {
        if self.armed.is_some_and(|a| a.expiration == expiration) {
            self.armed = None;
            self.dirty = true;
        }
        self.advance(now);
    }
}
