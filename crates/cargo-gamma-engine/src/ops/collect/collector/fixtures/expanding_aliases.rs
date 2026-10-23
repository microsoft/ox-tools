// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

type Generic0<T> = Generic1<(T, T)>;
type Generic1<T> = Generic2<(T, T)>;
type Generic2<T> = Generic3<(T, T)>;
type Generic3<T> = Generic4<(T, T)>;
type Generic4<T> = Generic5<(T, T)>;
type Generic5<T> = Generic6<(T, T)>;
type Generic6<T> = Generic7<(T, T)>;
type Generic7<T> = Generic8<(T, T)>;
type Generic8<T> = Generic9<(T, T)>;
type Generic9<T> = Generic10<(T, T)>;
type Generic10<T> = Generic11<(T, T)>;
type Generic11<T> = Generic12<(T, T)>;
type Generic12<T> = Generic13<(T, T)>;
type Generic13<T> = Generic14<(T, T)>;
type Generic14<T> = Generic15<(T, T)>;
type Generic15<T> = Generic16<(T, T)>;
type Generic16<T> = (T, T);

type Branch0<T> = (Branch1<T>, Branch1<T>);
type Branch1<T> = (Branch2<T>, Branch2<T>);
type Branch2<T> = (Branch3<T>, Branch3<T>);
type Branch3<T> = (Branch4<T>, Branch4<T>);
type Branch4<T> = (Branch5<T>, Branch5<T>);
type Branch5<T> = (Branch6<T>, Branch6<T>);
type Branch6<T> = (Branch7<T>, Branch7<T>);
type Branch7<T> = (Branch8<T>, Branch8<T>);
type Branch8<T> = (Branch9<T>, Branch9<T>);
type Branch9<T> = (Branch10<T>, Branch10<T>);
type Branch10<T> = (Branch11<T>, Branch11<T>);
type Branch11<T> = (Branch12<T>, Branch12<T>);
type Branch12<T> = (Branch13<T>, Branch13<T>);
type Branch13<T> = (Branch14<T>, Branch14<T>);
type Branch14<T> = (Branch15<T>, Branch15<T>);
type Branch15<T> = (Branch16<T>, Branch16<T>);
type Branch16<T> = (T, T);

fn generic(input: Generic0<u8>) -> Generic0<bool> {
    todo!()
}

fn branching(input: Branch0<u8>) -> Branch0<bool> {
    todo!()
}
