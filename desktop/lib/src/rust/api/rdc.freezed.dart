// GENERATED CODE - DO NOT MODIFY BY HAND
// coverage:ignore-file
// ignore_for_file: type=lint, type=warning, deprecated_member_use, deprecated_member_use_from_same_package
// ignore_for_file: unused_element, deprecated_member_use, deprecated_member_use_from_same_package, use_function_type_syntax_for_parameters, unnecessary_const, avoid_init_to_null, invalid_override_different_default_values_named, prefer_expression_function_bodies, annotate_overrides, invalid_annotation_target, unnecessary_question_mark

part of 'rdc.dart';

// **************************************************************************
// FreezedGenerator
// **************************************************************************

// GENERATED CODE - DO NOT MODIFY BY HAND
// dart format off
T _$identity<T>(T value) => value;
/// @nodoc
mixin _$SyncPhase {





@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is SyncPhase);
}


@override
int get hashCode => runtimeType.hashCode;

@override
String toString() {
    return 'SyncPhase()';
}


}

/// @nodoc
class $SyncPhaseCopyWith<$Res>  {
$SyncPhaseCopyWith(SyncPhase _, $Res Function(SyncPhase) __);
}


/// Adds pattern-matching-related methods to [SyncPhase].
extension SyncPhasePatterns on SyncPhase {
/// A variant of `map` that fallback to returning `orElse`.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case _:
///     return orElse();
/// }
/// ```

@optionalTypeArgs TResult maybeMap<TResult extends Object?>({TResult Function( SyncPhase_Started value)?  started,TResult Function( SyncPhase_Log value)?  log,TResult Function( SyncPhase_Prompt value)?  prompt,TResult Function( SyncPhase_PromptResolved value)?  promptResolved,TResult Function( SyncPhase_Idle value)?  idle,TResult Function( SyncPhase_Done value)?  done,TResult Function( SyncPhase_Error value)?  error,TResult Function( SyncPhase_Stopped value)?  stopped,required TResult orElse(),}){
final _that = this;
switch (_that) {
case SyncPhase_Started() when started != null:
return started(_that);case SyncPhase_Log() when log != null:
return log(_that);case SyncPhase_Prompt() when prompt != null:
return prompt(_that);case SyncPhase_PromptResolved() when promptResolved != null:
return promptResolved(_that);case SyncPhase_Idle() when idle != null:
return idle(_that);case SyncPhase_Done() when done != null:
return done(_that);case SyncPhase_Error() when error != null:
return error(_that);case SyncPhase_Stopped() when stopped != null:
return stopped(_that);case _:
  return orElse();

}
}
/// A `switch`-like method, using callbacks.
///
/// Callbacks receives the raw object, upcasted.
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case final Subclass2 value:
///     return ...;
/// }
/// ```

@optionalTypeArgs TResult map<TResult extends Object?>({required TResult Function( SyncPhase_Started value)  started,required TResult Function( SyncPhase_Log value)  log,required TResult Function( SyncPhase_Prompt value)  prompt,required TResult Function( SyncPhase_PromptResolved value)  promptResolved,required TResult Function( SyncPhase_Idle value)  idle,required TResult Function( SyncPhase_Done value)  done,required TResult Function( SyncPhase_Error value)  error,required TResult Function( SyncPhase_Stopped value)  stopped,}){
final _that = this;
switch (_that) {
case SyncPhase_Started():
return started(_that);case SyncPhase_Log():
return log(_that);case SyncPhase_Prompt():
return prompt(_that);case SyncPhase_PromptResolved():
return promptResolved(_that);case SyncPhase_Idle():
return idle(_that);case SyncPhase_Done():
return done(_that);case SyncPhase_Error():
return error(_that);case SyncPhase_Stopped():
return stopped(_that);}
}
/// A variant of `map` that fallback to returning `null`.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case _:
///     return null;
/// }
/// ```

@optionalTypeArgs TResult? mapOrNull<TResult extends Object?>({TResult? Function( SyncPhase_Started value)?  started,TResult? Function( SyncPhase_Log value)?  log,TResult? Function( SyncPhase_Prompt value)?  prompt,TResult? Function( SyncPhase_PromptResolved value)?  promptResolved,TResult? Function( SyncPhase_Idle value)?  idle,TResult? Function( SyncPhase_Done value)?  done,TResult? Function( SyncPhase_Error value)?  error,TResult? Function( SyncPhase_Stopped value)?  stopped,}){
final _that = this;
switch (_that) {
case SyncPhase_Started() when started != null:
return started(_that);case SyncPhase_Log() when log != null:
return log(_that);case SyncPhase_Prompt() when prompt != null:
return prompt(_that);case SyncPhase_PromptResolved() when promptResolved != null:
return promptResolved(_that);case SyncPhase_Idle() when idle != null:
return idle(_that);case SyncPhase_Done() when done != null:
return done(_that);case SyncPhase_Error() when error != null:
return error(_that);case SyncPhase_Stopped() when stopped != null:
return stopped(_that);case _:
  return null;

}
}
/// A variant of `when` that fallback to an `orElse` callback.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case _:
///     return orElse();
/// }
/// ```

@optionalTypeArgs TResult maybeWhen<TResult extends Object?>({TResult Function()?  started,TResult Function( String line)?  log,TResult Function( BigInt id,  PromptKindDto kind,  String question,  List<PromptChoice> keys)?  prompt,TResult Function( BigInt id)?  promptResolved,TResult Function( BigInt? nextPollSecs)?  idle,TResult Function( BigInt fileCount)?  done,TResult Function( String message)?  error,TResult Function()?  stopped,required TResult orElse(),}) {final _that = this;
switch (_that) {
case SyncPhase_Started() when started != null:
return started();case SyncPhase_Log() when log != null:
return log(_that.line);case SyncPhase_Prompt() when prompt != null:
return prompt(_that.id,_that.kind,_that.question,_that.keys);case SyncPhase_PromptResolved() when promptResolved != null:
return promptResolved(_that.id);case SyncPhase_Idle() when idle != null:
return idle(_that.nextPollSecs);case SyncPhase_Done() when done != null:
return done(_that.fileCount);case SyncPhase_Error() when error != null:
return error(_that.message);case SyncPhase_Stopped() when stopped != null:
return stopped();case _:
  return orElse();

}
}
/// A `switch`-like method, using callbacks.
///
/// As opposed to `map`, this offers destructuring.
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case Subclass2(:final field2):
///     return ...;
/// }
/// ```

@optionalTypeArgs TResult when<TResult extends Object?>({required TResult Function()  started,required TResult Function( String line)  log,required TResult Function( BigInt id,  PromptKindDto kind,  String question,  List<PromptChoice> keys)  prompt,required TResult Function( BigInt id)  promptResolved,required TResult Function( BigInt? nextPollSecs)  idle,required TResult Function( BigInt fileCount)  done,required TResult Function( String message)  error,required TResult Function()  stopped,}) {final _that = this;
switch (_that) {
case SyncPhase_Started():
return started();case SyncPhase_Log():
return log(_that.line);case SyncPhase_Prompt():
return prompt(_that.id,_that.kind,_that.question,_that.keys);case SyncPhase_PromptResolved():
return promptResolved(_that.id);case SyncPhase_Idle():
return idle(_that.nextPollSecs);case SyncPhase_Done():
return done(_that.fileCount);case SyncPhase_Error():
return error(_that.message);case SyncPhase_Stopped():
return stopped();}
}
/// A variant of `when` that fallback to returning `null`
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case _:
///     return null;
/// }
/// ```

@optionalTypeArgs TResult? whenOrNull<TResult extends Object?>({TResult? Function()?  started,TResult? Function( String line)?  log,TResult? Function( BigInt id,  PromptKindDto kind,  String question,  List<PromptChoice> keys)?  prompt,TResult? Function( BigInt id)?  promptResolved,TResult? Function( BigInt? nextPollSecs)?  idle,TResult? Function( BigInt fileCount)?  done,TResult? Function( String message)?  error,TResult? Function()?  stopped,}) {final _that = this;
switch (_that) {
case SyncPhase_Started() when started != null:
return started();case SyncPhase_Log() when log != null:
return log(_that.line);case SyncPhase_Prompt() when prompt != null:
return prompt(_that.id,_that.kind,_that.question,_that.keys);case SyncPhase_PromptResolved() when promptResolved != null:
return promptResolved(_that.id);case SyncPhase_Idle() when idle != null:
return idle(_that.nextPollSecs);case SyncPhase_Done() when done != null:
return done(_that.fileCount);case SyncPhase_Error() when error != null:
return error(_that.message);case SyncPhase_Stopped() when stopped != null:
return stopped();case _:
  return null;

}
}

}

/// @nodoc


class SyncPhase_Started extends SyncPhase {
  const SyncPhase_Started(): super._();
  






@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is SyncPhase_Started);
}


@override
int get hashCode => runtimeType.hashCode;

@override
String toString() {
    return 'SyncPhase.started()';
}


}




/// @nodoc


class SyncPhase_Log extends SyncPhase {
  const SyncPhase_Log({required this.line}): super._();
  

 final  String line;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$SyncPhase_LogCopyWith<SyncPhase_Log> get copyWith => _$SyncPhase_LogCopyWithImpl<SyncPhase_Log>(this, _$identity);



@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is SyncPhase_Log&&(identical(other.line, line) || other.line == line));
}


@override
int get hashCode {
    return Object.hash(runtimeType,line);
}

@override
String toString() {
    return 'SyncPhase.log(line: $line)';
}


}

/// @nodoc
abstract mixin class $SyncPhase_LogCopyWith<$Res> implements $SyncPhaseCopyWith<$Res> {
  factory $SyncPhase_LogCopyWith(SyncPhase_Log value, $Res Function(SyncPhase_Log) _then) = _$SyncPhase_LogCopyWithImpl;
@useResult
$Res call({
 String line
});




}
/// @nodoc
class _$SyncPhase_LogCopyWithImpl<$Res>
    implements $SyncPhase_LogCopyWith<$Res> {
  _$SyncPhase_LogCopyWithImpl(this._self, this._then);

  final SyncPhase_Log _self;
  final $Res Function(SyncPhase_Log) _then;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? line = null,}) {
  return _then(SyncPhase_Log(
line: null == line ? _self.line : line // ignore: cast_nullable_to_non_nullable
as String,
  ));
}


}

/// @nodoc


class SyncPhase_Prompt extends SyncPhase {
  const SyncPhase_Prompt({required this.id, required this.kind, required this.question, required  List<PromptChoice> keys}): _keys = keys,super._();
  

 final  BigInt id;
 final  PromptKindDto kind;
 final  String question;
 final  List<PromptChoice> _keys;
 List<PromptChoice> get keys {
  if (_keys is EqualUnmodifiableListView) return _keys;
  // ignore: implicit_dynamic_type
  return EqualUnmodifiableListView(_keys);
}


/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$SyncPhase_PromptCopyWith<SyncPhase_Prompt> get copyWith => _$SyncPhase_PromptCopyWithImpl<SyncPhase_Prompt>(this, _$identity);



@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is SyncPhase_Prompt&&(identical(other.id, id) || other.id == id)&&(identical(other.kind, kind) || other.kind == kind)&&(identical(other.question, question) || other.question == question)&&const DeepCollectionEquality().equals(other.keys, _keys));
}


@override
int get hashCode {
    return Object.hash(runtimeType,id,kind,question,const DeepCollectionEquality().hash(_keys));
}

@override
String toString() {
    return 'SyncPhase.prompt(id: $id, kind: $kind, question: $question, keys: $keys)';
}


}

/// @nodoc
abstract mixin class $SyncPhase_PromptCopyWith<$Res> implements $SyncPhaseCopyWith<$Res> {
  factory $SyncPhase_PromptCopyWith(SyncPhase_Prompt value, $Res Function(SyncPhase_Prompt) _then) = _$SyncPhase_PromptCopyWithImpl;
@useResult
$Res call({
 BigInt id, PromptKindDto kind, String question, List<PromptChoice> keys
});




}
/// @nodoc
class _$SyncPhase_PromptCopyWithImpl<$Res>
    implements $SyncPhase_PromptCopyWith<$Res> {
  _$SyncPhase_PromptCopyWithImpl(this._self, this._then);

  final SyncPhase_Prompt _self;
  final $Res Function(SyncPhase_Prompt) _then;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? id = null,Object? kind = null,Object? question = null,Object? keys = null,}) {
  return _then(SyncPhase_Prompt(
id: null == id ? _self.id : id // ignore: cast_nullable_to_non_nullable
as BigInt,kind: null == kind ? _self.kind : kind // ignore: cast_nullable_to_non_nullable
as PromptKindDto,question: null == question ? _self.question : question // ignore: cast_nullable_to_non_nullable
as String,keys: null == keys ? _self._keys : keys // ignore: cast_nullable_to_non_nullable
as List<PromptChoice>,
  ));
}


}

/// @nodoc


class SyncPhase_PromptResolved extends SyncPhase {
  const SyncPhase_PromptResolved({required this.id}): super._();
  

 final  BigInt id;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$SyncPhase_PromptResolvedCopyWith<SyncPhase_PromptResolved> get copyWith => _$SyncPhase_PromptResolvedCopyWithImpl<SyncPhase_PromptResolved>(this, _$identity);



@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is SyncPhase_PromptResolved&&(identical(other.id, id) || other.id == id));
}


@override
int get hashCode {
    return Object.hash(runtimeType,id);
}

@override
String toString() {
    return 'SyncPhase.promptResolved(id: $id)';
}


}

/// @nodoc
abstract mixin class $SyncPhase_PromptResolvedCopyWith<$Res> implements $SyncPhaseCopyWith<$Res> {
  factory $SyncPhase_PromptResolvedCopyWith(SyncPhase_PromptResolved value, $Res Function(SyncPhase_PromptResolved) _then) = _$SyncPhase_PromptResolvedCopyWithImpl;
@useResult
$Res call({
 BigInt id
});




}
/// @nodoc
class _$SyncPhase_PromptResolvedCopyWithImpl<$Res>
    implements $SyncPhase_PromptResolvedCopyWith<$Res> {
  _$SyncPhase_PromptResolvedCopyWithImpl(this._self, this._then);

  final SyncPhase_PromptResolved _self;
  final $Res Function(SyncPhase_PromptResolved) _then;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? id = null,}) {
  return _then(SyncPhase_PromptResolved(
id: null == id ? _self.id : id // ignore: cast_nullable_to_non_nullable
as BigInt,
  ));
}


}

/// @nodoc


class SyncPhase_Idle extends SyncPhase {
  const SyncPhase_Idle({this.nextPollSecs}): super._();
  

 final  BigInt? nextPollSecs;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$SyncPhase_IdleCopyWith<SyncPhase_Idle> get copyWith => _$SyncPhase_IdleCopyWithImpl<SyncPhase_Idle>(this, _$identity);



@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is SyncPhase_Idle&&(identical(other.nextPollSecs, nextPollSecs) || other.nextPollSecs == nextPollSecs));
}


@override
int get hashCode {
    return Object.hash(runtimeType,nextPollSecs);
}

@override
String toString() {
    return 'SyncPhase.idle(nextPollSecs: $nextPollSecs)';
}


}

/// @nodoc
abstract mixin class $SyncPhase_IdleCopyWith<$Res> implements $SyncPhaseCopyWith<$Res> {
  factory $SyncPhase_IdleCopyWith(SyncPhase_Idle value, $Res Function(SyncPhase_Idle) _then) = _$SyncPhase_IdleCopyWithImpl;
@useResult
$Res call({
 BigInt? nextPollSecs
});




}
/// @nodoc
class _$SyncPhase_IdleCopyWithImpl<$Res>
    implements $SyncPhase_IdleCopyWith<$Res> {
  _$SyncPhase_IdleCopyWithImpl(this._self, this._then);

  final SyncPhase_Idle _self;
  final $Res Function(SyncPhase_Idle) _then;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? nextPollSecs = freezed,}) {
  return _then(SyncPhase_Idle(
nextPollSecs: freezed == nextPollSecs ? _self.nextPollSecs : nextPollSecs // ignore: cast_nullable_to_non_nullable
as BigInt?,
  ));
}


}

/// @nodoc


class SyncPhase_Done extends SyncPhase {
  const SyncPhase_Done({required this.fileCount}): super._();
  

 final  BigInt fileCount;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$SyncPhase_DoneCopyWith<SyncPhase_Done> get copyWith => _$SyncPhase_DoneCopyWithImpl<SyncPhase_Done>(this, _$identity);



@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is SyncPhase_Done&&(identical(other.fileCount, fileCount) || other.fileCount == fileCount));
}


@override
int get hashCode {
    return Object.hash(runtimeType,fileCount);
}

@override
String toString() {
    return 'SyncPhase.done(fileCount: $fileCount)';
}


}

/// @nodoc
abstract mixin class $SyncPhase_DoneCopyWith<$Res> implements $SyncPhaseCopyWith<$Res> {
  factory $SyncPhase_DoneCopyWith(SyncPhase_Done value, $Res Function(SyncPhase_Done) _then) = _$SyncPhase_DoneCopyWithImpl;
@useResult
$Res call({
 BigInt fileCount
});




}
/// @nodoc
class _$SyncPhase_DoneCopyWithImpl<$Res>
    implements $SyncPhase_DoneCopyWith<$Res> {
  _$SyncPhase_DoneCopyWithImpl(this._self, this._then);

  final SyncPhase_Done _self;
  final $Res Function(SyncPhase_Done) _then;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? fileCount = null,}) {
  return _then(SyncPhase_Done(
fileCount: null == fileCount ? _self.fileCount : fileCount // ignore: cast_nullable_to_non_nullable
as BigInt,
  ));
}


}

/// @nodoc


class SyncPhase_Error extends SyncPhase {
  const SyncPhase_Error({required this.message}): super._();
  

 final  String message;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$SyncPhase_ErrorCopyWith<SyncPhase_Error> get copyWith => _$SyncPhase_ErrorCopyWithImpl<SyncPhase_Error>(this, _$identity);



@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is SyncPhase_Error&&(identical(other.message, message) || other.message == message));
}


@override
int get hashCode {
    return Object.hash(runtimeType,message);
}

@override
String toString() {
    return 'SyncPhase.error(message: $message)';
}


}

/// @nodoc
abstract mixin class $SyncPhase_ErrorCopyWith<$Res> implements $SyncPhaseCopyWith<$Res> {
  factory $SyncPhase_ErrorCopyWith(SyncPhase_Error value, $Res Function(SyncPhase_Error) _then) = _$SyncPhase_ErrorCopyWithImpl;
@useResult
$Res call({
 String message
});




}
/// @nodoc
class _$SyncPhase_ErrorCopyWithImpl<$Res>
    implements $SyncPhase_ErrorCopyWith<$Res> {
  _$SyncPhase_ErrorCopyWithImpl(this._self, this._then);

  final SyncPhase_Error _self;
  final $Res Function(SyncPhase_Error) _then;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? message = null,}) {
  return _then(SyncPhase_Error(
message: null == message ? _self.message : message // ignore: cast_nullable_to_non_nullable
as String,
  ));
}


}

/// @nodoc


class SyncPhase_Stopped extends SyncPhase {
  const SyncPhase_Stopped(): super._();
  






@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is SyncPhase_Stopped);
}


@override
int get hashCode => runtimeType.hashCode;

@override
String toString() {
    return 'SyncPhase.stopped()';
}


}




// dart format on
